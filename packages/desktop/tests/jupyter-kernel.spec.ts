import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { randomUUID } from "node:crypto";
import { JupyterKernel } from "../electron/jupyter-kernel";
import type { KernelOutput } from "../electron/jupyter-types";

vi.mock("../electron/env/userShellEnv", () => ({ getUserShellBaseEnv: async () => ({ ...process.env }) }));
const python = process.env.CRAB_TEST_JUPYTER_PYTHON;

describe.runIf(python)("reference Jupyter runtime ownership", () => {
  let root: string, kernel: JupyterKernel, outputs: KernelOutput[];
  const execute = (code: string) => kernel.execute(code, randomUUID());
  const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; } };
  beforeEach(async () => {
    root = await fs.mkdtemp(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "jupyter-owner-"));
    for (const name of ["TMPDIR", "TEMP", "TMP", "JUPYTER_RUNTIME_DIR", "JUPYTER_CONFIG_DIR"]) vi.stubEnv(name, root);
    outputs = [];
    kernel = new JupyterKernel("python3", root, python!, { ...process.env });
    kernel.setOutputHandler(output => outputs.push(output));
  });
  afterEach(async () => { await kernel.shutdown(); vi.unstubAllEnvs(); await fs.rm(root, { recursive: true, force: true }); });

  it("becomes ready in an explicit runtime directory and finishes after all immediate output", async () => {
    await kernel.start();
    const requestId = randomUUID();
    expect(await kernel.execute("import os\nos.write(1, b'native stdout is not the worker protocol\\n')\nfor i in range(50): print('CRAB_READY_' + str(i), flush=True)", requestId)).toEqual({ status: "ok" });
    const stream = outputs.filter(output => output.msgType === "stream");
    expect(stream.every(output => output.requestId === requestId)).toBe(true);
    expect(stream.map(output => output.content.text).join("")).toContain("CRAB_READY_49");
  }, 45_000);

  it("interrupts a busy kernel without discarding variables or its process", async () => {
    await kernel.start();
    const entered = path.join(root, "entered");
    const pending = execute(`import os,time\nretained = 42\nopen(${JSON.stringify(entered)}, 'w').write(str(os.getpid()))\nwhile True: time.sleep(0.01)`);
    await vi.waitFor(async () => expect(await fs.readFile(entered, "utf8")).toMatch(/^\d+$/), { timeout: 10_000 });
    const pid = Number(await fs.readFile(entered, "utf8"));
    await kernel.interrupt();
    expect(await pending).toEqual({ status: "error" });
    expect(outputs.some(output => output.content.ename === "KeyboardInterrupt")).toBe(true);
    expect(alive(pid)).toBe(true);
    expect(await execute("print(retained)")).toEqual({ status: "ok" });
    expect(outputs.some(output => output.content.text === "42\n")).toBe(true);
  }, 45_000);

  it("restarts only after retiring the old process and clears its variables", async () => {
    await kernel.start();
    const identity = path.join(root, "identity");
    await execute(`import os\nretained = 42\nopen(${JSON.stringify(identity)}, 'w').write(str(os.getpid()))`);
    const pid = Number(await fs.readFile(identity, "utf8"));
    await kernel.restart();
    expect(alive(pid)).toBe(false);
    expect(await execute("print('retained' in globals())")).toEqual({ status: "ok" });
    expect(outputs.some(output => output.content.text === "False\n")).toBe(true);
  }, 45_000);

  it("joins busy shutdown and removes owned connection files", async () => {
    await kernel.start();
    const identity = path.join(root, "identity");
    const pending = execute(`import os,time\nopen(${JSON.stringify(identity)}, 'w').write(str(os.getpid()))\nwhile True: time.sleep(0.01)`);
    const terminal = pending.catch(error => error as Error);
    await vi.waitFor(async () => expect(await fs.readFile(identity, "utf8")).toMatch(/^\d+$/), { timeout: 10_000 });
    const pid = Number(await fs.readFile(identity, "utf8"));
    await kernel.shutdown();
    await terminal;
    expect(alive(pid)).toBe(false);
    expect((await fs.readdir(root)).filter(name => name.startsWith("crab-kernel-"))).toEqual([]);
    expect(kernel.isAlive()).toBe(false);
  }, 45_000);

  it("retires a failed startup and remains safely disconnectable", async () => {
    kernel = new JupyterKernel("missing-crab-test-kernel", root, python!, { ...process.env });
    await expect(kernel.start()).rejects.toThrow(/No such kernel/);
    await kernel.shutdown();
    expect((await fs.readdir(root)).filter(name => name.startsWith("crab-kernel-"))).toEqual([]);
  }, 45_000);
});
