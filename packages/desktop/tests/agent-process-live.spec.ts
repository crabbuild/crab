import { describe, expect, it, vi } from "vitest";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { spawnLocalAgent } from "../electron/agent-process";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const alive = (pid: number): boolean => {
  try { process.kill(pid, 0); return true; } catch (error: unknown) {
    if ((error as NodeJS.ErrnoException).code === "ESRCH") return false;
    throw error;
  }
};

describe.skipIf(!liveRoot || process.platform === "win32")("local native process ownership", () => {
  it.each(["ignored", "inherited"] as const)("retires a descendant with %s stdio after its leader exits", async mode => {
    const root = await mkdtemp(path.join(liveRoot!, "native-owner-"));
    const owner = spawnLocalAgent(process.execPath, [path.resolve("tests/fixtures/workspace-writer.mjs"), root, mode], { cwd: root });
    owner.process.on("error", () => {});
    owner.process.stdout?.resume(); owner.process.stderr?.resume();
    let child: number | undefined;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    try {
      await vi.waitFor(async () => { child = Number(await readFile(path.join(root, "child.pid"), "utf8")); expect(child).toBeGreaterThan(0); }, { timeout: 10_000 });
      await Promise.race([owner.closed, new Promise<never>((_resolve, reject) => { timeout = setTimeout(() => reject(new Error("Native owner did not finish descendant closure")), 12_000); })]);
      const running = alive(child!);
      await writeFile(path.join(liveRoot!, `native-owner-${mode}.json`), JSON.stringify({ child, running }));
      expect(running).toBe(false);
    } finally {
      clearTimeout(timeout);
      owner.process.kill("SIGKILL");
      if (child && alive(child)) process.kill(child, "SIGKILL");
      await owner.closed.catch(() => {});
      if (child) await vi.waitFor(() => expect(alive(child!)).toBe(false), { timeout: 10_000 });
      await rm(root, { recursive: true, force: true });
    }
  }, 30_000);
});
