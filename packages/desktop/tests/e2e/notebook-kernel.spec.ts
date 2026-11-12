import { chooseSelect } from "../helpers/select";
import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, realpathSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";
import type {} from "../../src/ipc";

for (const end of ["disconnect", "quit"] as const) test(`local notebook executes, recovers, interrupts and restarts before busy ${end}`, async () => {
  const python = process.env.CRAB_TEST_JUPYTER_PYTHON;
  test.skip(!python, "Set CRAB_TEST_JUPYTER_PYTHON to a Python environment with Jupyter Client and ipykernel");
  test.setTimeout(end === "disconnect" ? 180_000 : 120_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-notebook-kernel-"));
  const selectedPython = end === "disconnect" ? path.join(root, "python", process.platform === "win32" ? "Scripts/python.exe" : "bin/python") : python!;
  const source = path.join(root, "source"); mkdirSync(source);
  const data = path.join(root, "data"), broken = path.join(data, "kernels", "failing"); mkdirSync(broken, { recursive: true });
  writeFileSync(path.join(broken, "kernel.json"), JSON.stringify({ argv: [path.join(root, "missing-kernel-executable"), "{connection_file}"], display_name: "Failing kernel fixture", language: "python" }));
  const delayed = path.join(data, "kernels", "delayed"); mkdirSync(delayed);
  const launchIdentity = path.join(root, "launch-identity.json"), blockReady = path.join(root, "block-ready"), launcher = path.join(root, "delayed.py");
  writeFileSync(launcher, `import os,json,pathlib,time\nidentity = pathlib.Path(${JSON.stringify(launchIdentity)})\ntemporary = identity.with_suffix('.tmp')\ntemporary.write_text(json.dumps({'pid':os.getpid(),'worker':os.getppid()}))\ntemporary.replace(identity)\nwhile pathlib.Path(${JSON.stringify(blockReady)}).exists(): time.sleep(0.01)\nfrom ipykernel.kernelapp import IPKernelApp\nIPKernelApp.launch_instance()`);
  writeFileSync(path.join(delayed, "kernel.json"), JSON.stringify({ argv: [selectedPython, "-u", launcher, "-f", "{connection_file}"], display_name: "Delayed readiness fixture", language: "python" }));
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, TMPDIR: root, TEMP: root, TMP: root, PIP_CACHE_DIR: path.join(root, "pip-cache"), JUPYTER_PATH: data, JUPYTER_RUNTIME_DIR: root, JUPYTER_CONFIG_DIR: path.join(root, "config"), CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  const codes = [
    "import os,json,sys\nretained = 42\nopen('identity.json','w').write(json.dumps({'pid':os.getpid(),'worker':os.getppid(),'cwd':os.getcwd(),'prefix':sys.prefix}))\nprint('CRAB_FAST_OUTPUT', flush=True)",
    "import os,time\nif os.path.exists('block'):\n    open('entered','w').write('BUSY')\n    while True: time.sleep(0.01)\nprint('CRAB_RETAINED_' + str(retained))",
    "import os\nif os.path.exists('fail'): raise ValueError('CRAB_EXPECTED_FAILURE')\nprint('CRAB_RUNALL_OK')",
    "open('completed','w').write(str(retained))\nprint('CRAB_COMPLETED')",
    // ipykernel 7 uses ContextVars for the wire parent. Carry the originating
    // context explicitly; a bare thread instead inherits the latest global parent.
    "import threading,time,pathlib,contextvars\ndef output_after_idle():\n    while not pathlib.Path('post-idle-release').exists(): time.sleep(0.01)\n    print('CRAB_AFTER_IDLE', flush=True)\n    pathlib.Path('post-idle-completed').write_text('done')\nthreading.Thread(target=contextvars.copy_context().run, args=(output_after_idle,), daemon=True).start()\nprint('CRAB_BEFORE_IDLE', flush=True)",
    "open('post-idle-release','w').write('release')\nprint('CRAB_RELEASED', flush=True)",
  ];
  const file = path.join(source, "analysis.ipynb");
  writeFileSync(file, JSON.stringify({ nbformat: 4, nbformat_minor: 5, metadata: {}, cells: codes.map(code => ({ cell_type: "code", source: code, metadata: {}, execution_count: null, outputs: [] })) }));
  writeFileSync(path.join(source, "note.txt"), "Other editor tab\n");
  git(["add", "."]); git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Kernel fixture"]);
  const index = readFileSync(path.join(source, ".git/index")), contents = readFileSync(file);
  const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; } };
  const identity = () => JSON.parse(readFileSync(path.join(source, "identity.json"), "utf8")) as { pid: number; worker: number; cwd: string; prefix: string };
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let child: ChildProcess | undefined;
  let stderr = "";
  try {
    if (end === "disconnect") execFileSync(python!, ["-m", "venv", path.join(root, "python")], { env });
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    child = application.process();
    child.stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    page.setDefaultTimeout(10_000);
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "notebook-kernel", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="analysis.ipynb"]').click();
    const panel = page.getByRole("dialog", { name: "Connect to kernel", exact: true });
    const chooseKernel = async (name: string) => {
      await page.getByRole("button", { name: "Connect Kernel", exact: true }).click();
      await expect(panel.getByRole("button", { name: "Check runtime", exact: true })).toBeEnabled();
      await panel.getByRole("textbox", { name: "Python executable", exact: true }).fill(selectedPython);
      await panel.getByRole("button", { name: "Check runtime", exact: true }).click();
      if (name === "failing" && end === "disconnect") {
        const install = panel.getByRole("button", { name: "Install Jupyter Client + ipykernel", exact: true });
        await expect(install).toBeVisible();
        await install.click();
        await expect(panel.getByRole("combobox", { name: "Kernel", exact: true })).toBeVisible({ timeout: 125_000 });
        const versions = execFileSync(selectedPython, ["-c", "import importlib.metadata,json,sys; print(json.dumps({'prefix':sys.prefix,'jupyter_client':importlib.metadata.version('jupyter_client'),'ipykernel':importlib.metadata.version('ipykernel')}))"], { env, encoding: "utf8" });
        writeFileSync(test.info().outputPath("installed-runtime.json"), versions);
      }
      await expect(panel.getByRole("combobox", { name: "Kernel", exact: true })).toBeVisible();
      await chooseSelect(
          panel.getByRole("combobox", { name: "Kernel", exact: true }),
          name,
        );
    };
    const repairNative = async () => {
      renameSync(path.join(source, ".git"), path.join(source, ".git-offline"));
      const agent = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).find(row => row && Number(row[2]) === application!.process().pid && row[3].includes("/agent/crab-desktop-agent "));
      if (!agent) throw new Error("Owned native agent not found");
      process.kill(Number(agent[1]), "SIGKILL");
      await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
      renameSync(path.join(source, ".git-offline"), path.join(source, ".git"));
      await page.getByRole("button", { name: "Retry reopening repository", exact: true }).click();
    };
    await chooseKernel("failing");
    await panel.getByRole("button", { name: "Start Local Kernel", exact: true }).click();
    await expect(page.getByRole("alert").filter({ hasText: "missing-kernel-executable" })).toBeVisible();
    await chooseKernel("python3");
    if (process.platform !== "win32" && end === "disconnect") {
      for (const action of ["cancel", "native-replacement"] as const) {
        await application.evaluate(async ({ app }) => {
          const load = process.getBuiltinModule("module").createRequire(app.getAppPath() + "/package.json");
          const runtime = load("./dist-electron/env/userShellEnv.cjs") as { getUserShellBaseEnv: () => Promise<NodeJS.ProcessEnv> };
          const original = runtime.getUserShellBaseEnv, environment = await original();
          let release!: (env: NodeJS.ProcessEnv) => void;
          const pending = new Promise<NodeJS.ProcessEnv>(resolve => { release = resolve; });
          const host = globalThis as unknown as { kernelAdmission?: { entered: boolean; release: () => void } };
          const state = { entered: false, release: () => { runtime.getUserShellBaseEnv = original; release(environment); delete host.kernelAdmission; } };
          host.kernelAdmission = state;
          runtime.getUserShellBaseEnv = () => { state.entered = true; return pending; };
        });
        await panel.getByRole("button", { name: "Start Local Kernel", exact: true }).click();
        await expect.poll(() => application!.evaluate(() => (globalThis as unknown as { kernelAdmission?: { entered: boolean } }).kernelAdmission?.entered)).toBe(true);
        if (action === "cancel") {
          await page.getByRole("button", { name: "Cancel kernel connection", exact: true }).click();
          await expect(page.getByRole("button", { name: "Connect Kernel", exact: true })).toBeVisible({ timeout: 5000 });
        } else {
          await repairNative();
          await expect(page.getByRole("status", { name: "Kernel status", exact: true })).toHaveText("starting");
        }
        await application.evaluate(() => { (globalThis as unknown as { kernelAdmission?: { release: () => void } }).kernelAdmission?.release(); });
        if (action === "native-replacement") {
          await expect(page.getByRole("alert").filter({ hasText: "native session" })).toBeVisible({ timeout: 5000 });
          writeFileSync(test.info().outputPath("after-stale-startup.txt"), await page.locator("body").innerText());
          await page.screenshot({ path: test.info().outputPath("after-stale-startup.png") });
        }
        const workers = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).filter(row => row && Number(row[2]) === child!.pid && row[3].includes("jupyter-runtime.py"));
        expect(workers).toHaveLength(0);
        await chooseKernel("python3");
      }
    }
    if (end === "disconnect") {
      await chooseSelect(
          panel.getByRole("combobox", { name: "Kernel", exact: true }),
          "delayed",
        );
      writeFileSync(blockReady, "blocked");
      await panel.getByRole("button", { name: "Start Local Kernel", exact: true }).click();
      const launched = () => JSON.parse(readFileSync(launchIdentity, "utf8")) as { pid: number; worker: number };
      await expect.poll(() => existsSync(launchIdentity)).toBe(true);
      const startup = launched();
      await page.getByRole("button", { name: "Cancel kernel connection", exact: true }).click();
      await expect(page.getByRole("button", { name: "Connect Kernel", exact: true })).toBeVisible({ timeout: 8000 });
      expect(alive(startup.pid) || alive(startup.worker)).toBe(false);
      expect(readdirSync(root).filter(name => name.startsWith("crab-kernel-"))).toEqual([]);
      rmSync(blockReady);
      await chooseKernel("delayed");
      await panel.getByRole("button", { name: "Start Local Kernel", exact: true }).click();
      await expect(page.getByRole("status", { name: "Kernel status", exact: true })).toHaveText("idle");
      const ready = launched(); writeFileSync(blockReady, "blocked");
      await page.getByRole("button", { name: "Restart kernel", exact: true }).click();
      await expect.poll(() => launched().pid).not.toBe(ready.pid);
      const restarting = launched();
      await page.screenshot({ path: test.info().outputPath("notebook-readiness-cancel.png") });
      await page.getByRole("button", { name: "Cancel kernel connection", exact: true }).click();
      await expect(page.getByRole("button", { name: "Connect Kernel", exact: true })).toBeVisible({ timeout: 8000 });
      expect([ready.pid, restarting.pid, restarting.worker].some(alive)).toBe(false);
      expect(readdirSync(root).filter(name => name.startsWith("crab-kernel-"))).toEqual([]);
      rmSync(blockReady);
      await chooseKernel("python3");
    }
    await panel.getByRole("button", { name: "Start Local Kernel", exact: true }).click();
    const status = page.getByRole("status", { name: "Kernel status", exact: true });
    await expect(status).toHaveText("idle");
    const output = (cell: number) => page.getByRole("region", { name: `Cell ${cell} output`, exact: true });
    const run = (cell: number) => page.getByRole("group", { name: `Notebook cell ${cell}`, exact: true }).getByRole("button", { name: "Run", exact: true });
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(output(4)).toContainText("CRAB_COMPLETED");
    await expect(output(6)).toContainText("CRAB_RELEASED");
    await expect(output(5)).toContainText("CRAB_AFTER_IDLE");
    await expect(output(6)).not.toContainText("CRAB_AFTER_IDLE");
    await expect(output(1)).toContainText("CRAB_FAST_OUTPUT");
    expect(readFileSync(path.join(source, "completed"), "utf8")).toBe("42");
    const before = identity(); expect(before.cwd).toBe(realpathSync(source));
    if (end === "disconnect") expect(realpathSync(before.prefix)).toBe(realpathSync(path.join(root, "python")));
    await page.locator('[role="treeitem"][data-item-path="note.txt"]').click();
    await page.locator('[role="treeitem"][data-item-path="analysis.ipynb"]').click();
    await expect(output(4)).toContainText("CRAB_COMPLETED");
    await expect(page.getByRole("button", { name: "Connect Kernel", exact: true })).toHaveCount(0);
    if (end === "disconnect") {
      await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
      await page.getByRole("tab", { name: "User", exact: true }).click();
      await chooseSelect(
          page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox"),
          "light",
        );
      await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
      await expect(output(4)).toContainText("CRAB_COMPLETED");
    }

    if (process.platform !== "win32") {
      await repairNative();
      await expect(output(4)).toContainText("CRAB_COMPLETED");
      await run(2).click();
      await expect(output(2)).toContainText("CRAB_RETAINED_42");
      expect(identity().pid).toBe(before.pid);
    }
    writeFileSync(path.join(source, "block"), "");
    await run(2).click();
    await expect.poll(() => existsSync(path.join(source, "entered"))).toBe(true);
    await page.locator('[role="treeitem"][data-item-path="note.txt"]').click();
    await page.locator('[role="treeitem"][data-item-path="analysis.ipynb"]').click();
    await expect(status).toHaveText("busy"); await expect(run(2)).toBeDisabled();
    await page.getByRole("button", { name: "Interrupt kernel", exact: true }).click();
    await expect(output(2)).toContainText("KeyboardInterrupt");
    await expect(run(2)).toBeEnabled(); expect(alive(before.pid)).toBe(true);
    rmSync(path.join(source, "block"));
    await page.getByRole("button", { name: "Restart kernel", exact: true }).click();
    await expect(status).toHaveText("idle"); expect(alive(before.pid)).toBe(false);
    await run(2).click(); await expect(output(2)).toContainText("NameError");
    rmSync(path.join(source, "completed")); writeFileSync(path.join(source, "fail"), "");
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(output(3)).toContainText("CRAB_EXPECTED_FAILURE");
    await expect(run(3)).toBeEnabled(); expect(existsSync(path.join(source, "completed"))).toBe(false);
    const current = identity();
    await application.evaluate(({ BrowserWindow }) => { BrowserWindow.getAllWindows()[0].setContentSize(960, 640); });
    const disconnect = page.getByRole("button", { name: "Disconnect kernel", exact: true });
    const bounds = await disconnect.boundingBox(); if (!bounds) throw new Error("Missing disconnect control");
    expect(bounds.x + bounds.width).toBeLessThanOrEqual(await page.evaluate(() => innerWidth));
    await page.screenshot({ path: test.info().outputPath("notebook-kernel-working.png") });
    writeFileSync(path.join(source, "block"), ""); rmSync(path.join(source, "entered"));
    await run(2).click(); await expect.poll(() => existsSync(path.join(source, "entered"))).toBe(true);
    if (end === "disconnect") { await disconnect.click(); await expect(page.getByRole("button", { name: "Connect Kernel", exact: true })).toBeVisible(); }
    else { const closed = new Promise<void>(resolve => { child!.once("exit", () => resolve()); }); await application.evaluate(({ app }) => { app.quit(); }); await closed; }
    expect(alive(current.pid)).toBe(false); expect(alive(current.worker)).toBe(false);
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
    expect(readFileSync(file).equals(contents)).toBe(true);
    writeFileSync(test.info().outputPath("notebook-kernel-proof.json"), JSON.stringify({ end, before, current, fastOutput: true, postIdleOutput: true, readinessCancellation: end === "disconnect", environmentCancellation: process.platform !== "win32" && end === "disconnect", sequentialRunAll: true, tabRetention: true, settingsRetention: end === "disconnect", nativeRepair: process.platform !== "win32", staleStartupRejected: process.platform !== "win32" && end === "disconnect", repeatedNativeFaultRecovery: process.platform !== "win32" && end === "disconnect", interruptKeepsKernel: true, restartRetiresKernel: true, runAllStopsOnError: true, busyCleanupJoined: true, indexAndNotebookUnchanged: true }, null, 2));
  } catch (error) {
    writeFileSync(test.info().outputPath("primary-failure.json"), JSON.stringify({ message: String(error), stack: error instanceof Error ? error.stack : null }, null, 2));
    throw error;
  } finally {
    if (existsSync(path.join(source, ".git-offline"))) renameSync(path.join(source, ".git-offline"), path.join(source, ".git"));
    if (application && child?.exitCode === null) {
      await application.evaluate(() => { (globalThis as unknown as { kernelAdmission?: { release: () => void } }).kernelAdmission?.release(); });
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
