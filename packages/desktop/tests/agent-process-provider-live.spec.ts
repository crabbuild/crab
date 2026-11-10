import { describe, expect, it, vi } from "vitest";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import path from "node:path";
import type { AgentEvent } from "../electron/orchestrator-adapters";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const alive = (pid: number): boolean => {
  try { process.kill(pid, 0); return true; } catch (error: unknown) {
    if ((error as NodeJS.ErrnoException).code === "ESRCH") return false;
    throw error;
  }
};
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

describe.skipIf(!liveRoot || process.platform === "win32")("native tool writer cancellation", () => {
  it.each(["codex", "claude-code"] as const)("%s closes actual shell writers before acknowledging cancellation", async provider => {
    const root = await realpath(await mkdtemp(path.join(liveRoot!, "native-tool-writer-")));
    const command = [process.execPath, path.resolve("tests/fixtures/workspace-writer.mjs"), root].map(quote).join(" ");
    const adapter = provider === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent(event => {
      events.push(event);
      const approval = event.approval;
      if (!approval) return;
      let requested = approval.detail?.split("\n\nReason:")[0];
      if (provider === "claude-code") {
        try { requested = (JSON.parse(approval.detail ?? "{}") as { command?: string }).command; } catch { requested = undefined; }
      }
      const allowed = approval.kind === "command" && (requested === command || ["/bin/zsh", "/bin/bash", "/bin/sh"].some(shell => requested === `${shell} -lc ${quote(command)}`));
      adapter.respondApproval(approval.requestId, allowed ? "allow_once" : "deny");
    });
    let pids: number[] = [];
    let timer: ReturnType<typeof setTimeout> | undefined;
    let succeeded = false;
    try {
      await promisify(execFile)("git", ["init", "--initial-branch=main", root]);
      await adapter.spawn(`Run exactly this command once, in the foreground, using your shell tool: ${command}\nIt writes only fixture files in this workspace and intentionally waits until the user cancels. Do not change it, background it, use other tools, or use subagents.`, root);
      await vi.waitFor(async () => {
        if (events.some(event => event.type === "done")) throw new Error(events.find(event => event.error)?.error ?? "Native turn ended before the writer started");
        expect((await readFile(path.join(root, "child.writes"), "utf8")).length).toBeGreaterThan(0);
      }, { timeout: 90_000, interval: 100 });
      pids = await Promise.all(["parent", "child"].map(async role => Number(await readFile(path.join(root, `${role}.pid`), "utf8"))));
      await Promise.race([adapter.cancel(), new Promise<never>((_resolve, reject) => { timer = setTimeout(() => reject(new Error("Native writer cancellation timed out")), 30_000); })]);
      expect(pids.map(alive)).toEqual([false, false]);
      succeeded = true;
    } finally {
      clearTimeout(timer);
      await writeFile(path.join(liveRoot!, `${provider}-native-tool-cancel.json`), JSON.stringify({ root, pids, alive: pids.map(alive), events }, null, 2));
      for (const role of ["parent", "child"]) {
        const pid = Number(await readFile(path.join(root, `${role}.pid`), "utf8").catch(() => "0"));
        if (pid && alive(pid)) process.kill(pid, "SIGKILL");
      }
      await adapter.cancel().catch(() => {});
      if (succeeded) await rm(root, { recursive: true, force: true });
    }
  }, 150_000);
});
