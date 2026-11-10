"""Own a local Jupyter kernel through the installed reference client."""

import asyncio
import json
import queue
import subprocess
import sys
import tempfile
import threading

from jupyter_client import AsyncKernelManager
from jupyter_client.kernelspec import KernelSpecManager


def emit(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=True) + "\n")
    sys.stdout.flush()


def reply(request_id, result=None, error=None):
    emit({"kind": "reply", "id": request_id, "result": result, "error": error})


async def run():
    commands = asyncio.Queue()
    loop = asyncio.get_running_loop()

    def read_commands():
        try:
            for line in sys.stdin:
                loop.call_soon_threadsafe(commands.put_nowait, json.loads(line))
        finally:
            # A closed Electron pipe still owns kernel shutdown, including
            # when the application exits without an explicit shutdown request.
            loop.call_soon_threadsafe(commands.put_nowait, None)

    threading.Thread(target=read_commands, daemon=True).start()
    manager = None
    client = None
    jobs = {}
    routes = {}
    cells = {}
    request = None
    launch = None
    readiness = None
    ready_request = None

    async def wait_ready():
        nonlocal client
        # A provisioner launch/restart must reach its terminal state before
        # shutdown. Cancelling it can leave KernelManager.ready unresolved.
        await asyncio.shield(launch)
        client = manager.client()
        client.start_channels()
        await client.wait_for_ready(timeout=30)

    with tempfile.TemporaryDirectory(prefix="crab-kernel-") as directory:
        try:
            while True:
                if readiness is not None and readiness.done():
                    try:
                        readiness.result()
                    except Exception as error:
                        reply(ready_request, error=str(error))
                        readiness = None
                        break
                    reply(ready_request)
                    emit({"kind": "status", "status": "idle"})
                    readiness = None
                try:
                    request = await asyncio.wait_for(commands.get(), timeout=0.01)
                except asyncio.TimeoutError:
                    request = False
                if request is None:
                    break
                if request:
                    method = request["method"]
                    params = request.get("params", {})
                    request_id = request["id"]
                    try:
                        if method == "start":
                            manager = AsyncKernelManager(kernel_name=params["kernelName"], connection_file=directory + "/connection.json")
                            # The worker's stdout is the control protocol;
                            # native kernel writes must not enter that pipe.
                            launch = asyncio.create_task(manager.start_kernel(cwd=params["cwd"], stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=sys.stderr))
                            readiness = asyncio.create_task(wait_ready())
                            ready_request = request_id
                        elif readiness is not None:
                            raise RuntimeError("Kernel readiness is still pending")
                        elif method == "execute":
                            msg_id = client.execute(params["code"], allow_stdin=False, stop_on_error=True)
                            previous = cells.get(params["cell"])
                            routes.pop(previous, None)
                            cells[params["cell"]] = msg_id
                            routes[msg_id] = request_id
                            jobs[msg_id] = {"id": request_id, "reply": None, "idle": False}
                        elif method == "interrupt":
                            await manager.interrupt_kernel()
                            reply(request_id)
                        elif method == "restart":
                            for job in jobs.values():
                                reply(job["id"], error="Kernel restarted before execution completed")
                            jobs.clear()
                            routes.clear()
                            cells.clear()
                            client.stop_channels()
                            launch = asyncio.create_task(manager.restart_kernel(now=False))
                            readiness = asyncio.create_task(wait_ready())
                            ready_request = request_id
                        else:
                            raise ValueError("Unknown kernel operation")
                    except Exception as error:
                        reply(request_id, error=str(error))
                        if method in ("start", "restart"):
                            break

                if client is None or readiness is not None:
                    continue
                if not await manager.is_alive():
                    raise RuntimeError("The kernel process exited")
                # Drain both channels without starving interrupt/shutdown
                # admission. Execution completes only after its reply AND idle.
                for channel in (client.iopub_channel, client.shell_channel):
                    for _ in range(100):
                        try:
                            message = await channel.get_msg(timeout=0.005)
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
                        elif parent in routes:
                            emit({"kind": "output", "requestId": routes[parent], "msgType": kind, "content": content})
                        if job and job["idle"] and job["reply"] is not None:
                            reply(job["id"], {"status": job["reply"]})
                            # Idle completes the request, not its output area.
                            # Keep only each cell's latest route for late output.
                            del jobs[parent]
        finally:
            # KernelManager joins its provisioner and removes connection data;
            # the worker must remain alive until this cleanup acknowledges.
            try:
                if launch is not None:
                    await asyncio.gather(launch, return_exceptions=True)
                if readiness is not None:
                    readiness.cancel()
                    await asyncio.gather(readiness, return_exceptions=True)
                    reply(ready_request, error="Kernel disconnected during readiness")
                if manager is not None:
                    if manager.has_kernel:
                        await manager.shutdown_kernel(now=False)
                    else:
                        # Failed kernelspec admission creates no process;
                        # release its resources without a shutdown request.
                        await manager.cleanup_resources()
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
            asyncio.run(run())
        except Exception as error:
            emit({"kind": "failed", "error": str(error)})
            sys.exit(1)
