import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

test("a remote notebook owns its authenticated Jupyter Server kernel", async () => {
  const python = process.env.CRAB_TEST_JUPYTER_PYTHON;
  test.skip(!python, "Set CRAB_TEST_JUPYTER_PYTHON to a Python environment with Jupyter Server and ipykernel");
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-jupyter-server-"));
  const source = path.join(root, "repository"), serverRoot = path.join(root, "server");
  mkdirSync(source);
  mkdirSync(serverRoot);
  const token = randomUUID();
  const portFile = path.join(root, "port"), hold = path.join(root, "hold-create");
  const env = {
    ...process.env, TMPDIR: root, TMP: root, TEMP: root, JUPYTER_TOKEN: token,
    JUPYTER_RUNTIME_DIR: path.join(root, "runtime"), JUPYTER_CONFIG_DIR: path.join(root, "config"),
    CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1",
  };
  writeFileSync(env.GIT_CONFIG_GLOBAL, "");
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  const codes = [
    "import os,json,pathlib\ncount = globals().get('count', 0) + 1\npathlib.Path('effect.json').write_text(json.dumps({'pid': os.getpid(), 'count': count}))\nprint('CRAB_REMOTE_' + str(count), flush=True)",
    "import time,pathlib\npathlib.Path('busy').write_text('running')\nwhile True: time.sleep(0.1)",
  ];
  const notebook = path.join(source, "remote.ipynb");
  const contents = JSON.stringify({
    nbformat: 4, nbformat_minor: 5, metadata: {},
    cells: codes.map(code => ({ cell_type: "code", source: code, metadata: {}, execution_count: null, outputs: [] })),
  });
  writeFileSync(notebook, contents);
  git(["add", "."]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Remote notebook fixture"]);
  const index = readFileSync(path.join(source, ".git/index"));
  const server = spawn(python!, ["-u", "-c", `
import asyncio
import pathlib
import socket
from jupyter_server.serverapp import ServerApp
from jupyter_server.services.kernels.kernelmanager import AsyncMappingKernelManager

class HeldKernelManager(AsyncMappingKernelManager):
    async def start_kernel(self, *args, **kwargs):
        kernel_id = await super().start_kernel(*args, **kwargs)
        while pathlib.Path(${JSON.stringify(hold)}).exists():
            await asyncio.sleep(0.01)
        return kernel_id

with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
app = ServerApp(ip="127.0.0.1", port=port, port_retries=0, base_url="/desktop/",
                root_dir=${JSON.stringify(serverRoot)}, open_browser=False, allow_root=True,
                kernel_manager_class=HeldKernelManager, log_level="WARN")
app.initialize([])
pathlib.Path(${JSON.stringify(portFile)}).write_text(str(port))
app.start()
`], { env, cwd: serverRoot, stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  server.stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-24_000); });
  const serverClosed = once(server, "close");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let appProcess: ChildProcess | undefined;
  let base = "";
  const request = (suffix = "", method = "GET") => fetch(base + "api/kernels" + suffix, {
    method, headers: { Authorization: "token " + token }, signal: AbortSignal.timeout(4000),
    body: method === "POST" ? "{}" : undefined,
  });
  const kernels = async () => {
    const response = await request();
    if (!response.ok) throw new Error("Kernel inventory returned HTTP " + response.status);
    return await response.json() as Array<{ id: string }>;
  };
  try {
    await expect.poll(() => {
      if (server.exitCode !== null) throw new Error("Jupyter Server fixture exited during startup");
      return existsSync(portFile) && /^\d+$/.test(readFileSync(portFile, "utf8"));
    }, { timeout: 15_000 }).toBe(true);
    base = `http://127.0.0.1:${readFileSync(portFile, "utf8")}/desktop/`;
    const siblingResponse = await request("", "POST");
    expect(siblingResponse.status).toBe(201);
    const { id: sibling } = await siblingResponse.json() as { id: string };
    const appEnv = Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string"));
    delete appEnv.JUPYTER_TOKEN;
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: appEnv });
    appProcess = application.process();
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "remote-notebook", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="remote.ipynb"]').click();
    const connect = page.getByRole("button", { name: "Connect Kernel", exact: true });
    const panel = page.getByRole("dialog", { name: "Connect to kernel", exact: true });
    const status = page.getByRole("status", { name: "Kernel status", exact: true });
    const connectServer = async (credential: string, name = "") => {
      await connect.click();
      await expect(panel.getByRole("button", { name: "Check runtime", exact: true })).toBeEnabled();
      await panel.getByRole("textbox", { name: "Python executable", exact: true }).fill(python!);
      await panel.getByRole("button", { name: "Check runtime", exact: true }).click();
      await panel.getByLabel("Jupyter server URL", { exact: true }).fill(base + "?token=" + credential);
      await panel.getByLabel("Remote kernel name", { exact: true }).fill(name);
      await panel.getByRole("button", { name: "Connect to server", exact: true }).click();
    };
    const owned = async () => (await kernels()).filter(kernel => kernel.id !== sibling);
    await connectServer("invalid-fixture-token");
    await expect(page.getByRole("alert").filter({ hasText: "denied access" })).toBeVisible();
    expect(await owned()).toEqual([]);
    writeFileSync(hold, "");
    await connectServer(token);
    await expect.poll(async () => (await owned()).length).toBe(1);
    await page.getByRole("button", { name: "Cancel kernel connection", exact: true }).click();
    await expect(status).toHaveText("stopping");
    rmSync(hold);
    await expect(connect).toBeVisible({ timeout: 10_000 });
    expect(await owned()).toEqual([]);

    await connectServer(token, "python3");
    await expect(status).toHaveText("idle", { timeout: 15_000 });
    const active = (await owned())[0].id;
    const run = (cell: number) => page.getByRole("group", { name: `Notebook cell ${cell}`, exact: true }).getByRole("button", { name: "Run", exact: true });
    const output = (cell: number) => page.getByRole("region", { name: `Cell ${cell} output`, exact: true });
    const effect = path.join(serverRoot, "effect.json");
    await run(1).click();
    await expect(output(1)).toContainText("CRAB_REMOTE_1");
    const original = JSON.parse(readFileSync(effect, "utf8")) as { pid: number; count: number };
    expect(existsSync(path.join(source, "effect.json"))).toBe(false);
    await run(2).click();
    await expect.poll(() => existsSync(path.join(serverRoot, "busy"))).toBe(true);
    await page.getByRole("button", { name: "Interrupt kernel", exact: true }).click();
    await expect(output(2)).toContainText("KeyboardInterrupt");
    await expect(run(2)).toBeEnabled();
    expect((await owned())[0].id).toBe(active);
    await page.getByRole("button", { name: "Restart kernel", exact: true }).click();
    await expect(status).toHaveText("idle", { timeout: 20_000 });
    await run(1).click();
    await expect(output(1)).toContainText("CRAB_REMOTE_1");
    expect(JSON.parse(readFileSync(effect, "utf8")).pid).not.toBe(original.pid);
    expect((await owned())[0].id).toBe(active);
    rmSync(path.join(serverRoot, "busy"));
    await run(2).click();
    await expect.poll(() => existsSync(path.join(serverRoot, "busy"))).toBe(true);
    await page.getByRole("button", { name: "Disconnect kernel", exact: true }).click();
    await expect(connect).toBeVisible({ timeout: 10_000 });
    expect((await kernels()).map(kernel => kernel.id)).toEqual([sibling]);
    await connectServer(token);
    await expect(status).toHaveText("idle", { timeout: 15_000 });
    const exited = once(appProcess, "exit");
    await application.evaluate(({ app }) => app.quit());
    expect((await exited)[0]).toBe(0);
    expect((await kernels()).map(kernel => kernel.id)).toEqual([sibling]);
    expect(readFileSync(notebook, "utf8")).toBe(contents);
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
  } finally {
    rmSync(hold, { force: true });
    try {
      if (appProcess?.exitCode === null && appProcess.signalCode === null) await application?.close();
    } finally {
      if (base && server.exitCode === null) {
        for (const kernel of await kernels().catch(() => [])) await request("/" + kernel.id, "DELETE").catch(() => {});
      }
      if (server.exitCode === null) server.kill();
      await serverClosed;
      writeFileSync(test.info().outputPath("jupyter-server.log"), stderr.replaceAll(token, "[fixture token]"));
      rmSync(root, { recursive: true, force: true });
    }
  }
});
