"""Own a local or server-hosted Jupyter kernel for one notebook."""

import asyncio
import json
import queue
import re
import struct
import subprocess
import sys
import tempfile
import threading
from urllib.parse import parse_qs, urlsplit, urlunsplit

from jupyter_client import AsyncKernelManager
from jupyter_client.kernelspec import KernelSpecManager
from jupyter_client.session import Session
from tornado.httpclient import HTTPClientError, HTTPRequest
from tornado.simple_httpclient import SimpleAsyncHTTPClient
from tornado.websocket import websocket_connect


def emit(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=True) + "\n")
    sys.stdout.flush()


def reply(request_id, result=None, error=None):
    emit({"kind": "reply", "id": request_id, "result": result, "error": error})


class LocalKernel:
    def __init__(self, name, cwd, directory):
        self.cwd = cwd
        self.manager = AsyncKernelManager(kernel_name=name, connection_file=directory + "/connection.json")
        self.client = None

    async def launch(self, restart=False):
        if restart:
            self.client.stop_channels()
            await self.manager.restart_kernel(now=False)
            return
        # Keep native kernel writes out of the worker's stdout protocol.
        await self.manager.start_kernel(cwd=self.cwd, stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=sys.stderr)

    async def ready(self):
        self.client = self.manager.client()
        self.client.start_channels()
        await self.client.wait_for_ready(timeout=30)

    async def execute(self, code):
        return self.client.execute(code, allow_stdin=False, stop_on_error=True)

    async def interrupt(self):
        await self.manager.interrupt_kernel()

    async def messages(self):
        if not await self.manager.is_alive():
            raise RuntimeError("The kernel process exited")
        for channel in (self.client.iopub_channel, self.client.shell_channel):
            for _ in range(100):
                try:
                    yield await channel.get_msg(timeout=0.005)
                except queue.Empty:
                    break

    async def close(self):
        try:
            if self.manager.has_kernel:
                await self.manager.shutdown_kernel(now=False)
            else:
                await self.manager.cleanup_resources()
        finally:
            if self.client is not None:
                self.client.stop_channels()


class ServerKernel:
    def __init__(self, name, url):
        try:
            endpoint = urlsplit(url)
            query = parse_qs(endpoint.query, keep_blank_values=True)
            if (endpoint.scheme not in ("http", "https") or not endpoint.hostname
                    or endpoint.username is not None or endpoint.fragment or "\\" in url
                    or set(query) - {"token"} or len(query.get("token", [])) > 1):
                raise ValueError()
            token = query.get("token", [""])[0]
            if len(token) > 4096 or any(not 33 <= ord(char) <= 126 for char in token):
                raise ValueError()
        except ValueError:
            raise ValueError("Enter the Jupyter server base URL with an optional token") from None
        if endpoint.scheme == "http" and endpoint.hostname not in ("localhost", "127.0.0.1", "::1"):
            raise ValueError("Use HTTPS for a non-local Jupyter server")
        self.kernels = urlunsplit((endpoint.scheme, endpoint.netloc, endpoint.path.rstrip("/") + "/api/kernels", "", ""))
        self.headers = {"Authorization": "token " + token} if token else {}
        self.name = name
        self.http = SimpleAsyncHTTPClient(force_instance=True, max_body_size=1024 * 1024)
        self.kernel = None
        self.socket = None
        self.connecting = None
        self.session = Session()

    async def request(self, url, method, expected, body=None):
        headers = {**self.headers, "Content-Type": "application/json"} if body is not None else self.headers
        request = HTTPRequest(url, method=method, headers=headers, body=body,
                              follow_redirects=False, connect_timeout=5, request_timeout=15)
        try:
            response = await self.http.fetch(request, raise_error=False)
        except Exception:
            raise RuntimeError(f"Jupyter server did not acknowledge {method}") from None
        if response.code in (401, 403):
            raise RuntimeError("Jupyter server denied access. Check the URL and token.")
        if response.code not in expected:
            raise RuntimeError(f"Jupyter server {method} returned HTTP {response.code}. Check the server base URL.")
        return response.body

    async def launch(self, restart=False):
        if restart:
            self.socket.close()
            self.socket = None
            await self.request(self.kernel + "/restart", "POST", (200,), "{}")
            return
        body = json.dumps({"name": self.name} if self.name else {})
        response = await self.request(self.kernels, "POST", (201,), body)
        try:
            kernel_id = json.loads(response)["id"]
            if not re.fullmatch(r"[A-Za-z0-9_-]+", kernel_id):
                raise ValueError()
        except (TypeError, ValueError, KeyError):
            raise RuntimeError("Jupyter server returned an invalid kernel identity") from None
        self.kernel = self.kernels + "/" + kernel_id

    async def ready(self):
        # The query identifies the channel session. Credentials stay in headers.
        self.session = Session()
        url = "ws" + self.kernel[4:] + "/channels?session_id=" + self.session.session
        request = HTTPRequest(url, headers=self.headers, connect_timeout=5, request_timeout=10)
        self.connecting = websocket_connect(request, ping_interval=15, ping_timeout=10, max_message_size=16 * 1024 * 1024)
        try:
            self.socket = await asyncio.shield(self.connecting)
        except HTTPClientError as error:
            raise RuntimeError(f"Jupyter kernel channels failed to connect (HTTP {error.code})") from None
        except (OSError, ValueError):
            raise RuntimeError("Jupyter kernel channels failed to connect") from None
        self.connecting = None
        request_id = await self.send("kernel_info_request", {})
        try:
            await asyncio.wait_for(self.wait_for_reply(request_id), timeout=20)
        except asyncio.TimeoutError:
            raise RuntimeError("The Jupyter server kernel did not become ready") from None

    async def wait_for_reply(self, request_id):
        reply_received = idle = False
        while not (reply_received and idle):
            message = await self.read()
            if message["parent_header"].get("msg_id") != request_id:
                continue
            kind = message["header"]["msg_type"]
            reply_received |= message["channel"] == "shell" and kind == "kernel_info_reply"
            idle |= message["channel"] == "iopub" and kind == "status" and message["content"]["execution_state"] == "idle"

    async def send(self, kind, content):
        message = self.session.msg(kind, content)
        message["channel"] = "shell"
        try:
            await asyncio.wait_for(self.socket.write_message(self.session.pack(message).decode("utf8")), timeout=10)
        except Exception:
            raise RuntimeError("Could not send a request to the Jupyter kernel") from None
        return message["header"]["msg_id"]

    async def execute(self, code):
        return await self.send("execute_request", {
            "code": code, "silent": False, "store_history": True,
            "user_expressions": {}, "allow_stdin": False, "stop_on_error": True,
        })

    async def interrupt(self):
        await self.request(self.kernel + "/interrupt", "POST", (204,), "{}")

    async def read(self):
        frame = await self.socket.read_message()
        if frame is None:
            raise RuntimeError("The Jupyter kernel connection closed")
        try:
            if isinstance(frame, bytes):
                # Default protocol binary frames place the JSON envelope first.
                # Binary buffers belong to widgets, which this viewer does not render.
                count, first = struct.unpack_from("!II", frame)
                end = struct.unpack_from("!I", frame, 8)[0] if count > 1 else len(frame)
                if count < 1 or first != 4 * (count + 1) or not first < end <= len(frame):
                    raise ValueError()
                frame = frame[first:end]
            message = json.loads(frame)
            if (not isinstance(message["channel"], str)
                    or not isinstance(message["header"]["msg_type"], str)
                    or not isinstance(message["parent_header"], dict)
                    or not isinstance(message["content"], dict)):
                raise ValueError()
            return message
        except (KeyError, TypeError, ValueError, struct.error):
            raise RuntimeError("Jupyter server sent an invalid kernel message") from None

    async def messages(self):
        for _ in range(100):
            try:
                message = await asyncio.wait_for(self.read(), timeout=0.005)
            except asyncio.TimeoutError:
                break
            if message["channel"] in ("iopub", "shell"):
                yield message

    async def close(self):
        try:
            if self.socket is not None:
                self.socket.close()
            if self.kernel is not None:
                await self.request(self.kernel, "DELETE", (204, 404))
        finally:
            if self.connecting is not None:
                # A cancelled readiness waiter may still own an upgrade in flight.
                result, = await asyncio.gather(self.connecting, return_exceptions=True)
                if not isinstance(result, BaseException):
                    result.close()
            self.http.close()


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
    kernel = None
    jobs = {}
    routes = {}
    cells = {}
    request = None
    launch = None
    readiness = None
    ready_request = None

    async def wait_ready():
        # Both provisioner launch and server creation must acknowledge before
        # shutdown can release the resource they admitted.
        await asyncio.shield(launch)
        await kernel.ready()

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
                            if "serverUrl" in params:
                                kernel = ServerKernel(params["kernelName"], params["serverUrl"])
                            else:
                                kernel = LocalKernel(params["kernelName"], params["cwd"], directory)
                            launch = asyncio.create_task(kernel.launch())
                            readiness = asyncio.create_task(wait_ready())
                            ready_request = request_id
                        elif readiness is not None:
                            raise RuntimeError("Kernel readiness is still pending")
                        elif method == "execute":
                            msg_id = await kernel.execute(params["code"])
                            previous = cells.get(params["cell"])
                            routes.pop(previous, None)
                            cells[params["cell"]] = msg_id
                            routes[msg_id] = request_id
                            jobs[msg_id] = {"id": request_id, "reply": None, "idle": False}
                        elif method == "interrupt":
                            await kernel.interrupt()
                            reply(request_id)
                        elif method == "restart":
                            for job in jobs.values():
                                reply(job["id"], error="Kernel restarted before execution completed")
                            jobs.clear()
                            routes.clear()
                            cells.clear()
                            launch = asyncio.create_task(kernel.launch(restart=True))
                            readiness = asyncio.create_task(wait_ready())
                            ready_request = request_id
                        else:
                            raise ValueError("Unknown kernel operation")
                    except Exception as error:
                        reply(request_id, error=str(error))
                        if method in ("start", "restart"):
                            break

                if kernel is None or readiness is not None:
                    continue
                # Drain both channels without starving interrupt/shutdown
                # admission. Execution completes only after its reply AND idle.
                async for message in kernel.messages():
                    parent = message["parent_header"].get("msg_id")
                    job = jobs.get(parent)
                    kind = message["header"]["msg_type"]
                    content = message["content"]
                    if kind == "status":
                        status = content["execution_state"]
                        if status in ("dead", "restarting"):
                            raise RuntimeError("The kernel stopped during execution")
                        emit({"kind": "status", "status": status})
                        if job and status == "idle":
                            job["idle"] = True
                    elif job and kind == "execute_reply":
                        job["reply"] = content["status"]
                    elif parent in routes:
                        emit({"kind": "output", "requestId": routes[parent], "msgType": kind, "content": content})
                    if job and job["idle"] and job["reply"] is not None:
                        reply(job["id"], {"status": job["reply"]})
                        # Keep the latest route so late output still has its cell.
                        del jobs[parent]
        except Exception as error:
            emit({"kind": "failed", "error": str(error)})
        finally:
            if readiness is not None:
                readiness.cancel()
                await asyncio.gather(readiness, return_exceptions=True)
                reply(ready_request, error="Kernel disconnected during readiness")
            if launch is not None:
                await asyncio.gather(launch, return_exceptions=True)
            if kernel is not None:
                await kernel.close()
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
