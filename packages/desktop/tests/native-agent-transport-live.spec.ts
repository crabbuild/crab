import { describe, expect, it, vi } from "vitest";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { homedir } from "node:os";
import path from "node:path";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";
import { getExpandedEnv, type AgentEvent } from "../electron/orchestrator-adapters";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";
import { terminalSshServer } from "./fixtures/ssh-terminal-server";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;

describe.skipIf(!liveRoot || process.platform === "win32")("native protocols through the owned sidecar transport", () => {
  it.each((["codex", "claude-code"] as const).flatMap(provider => ["models", "turn"].map(mode => ({ provider, mode }))))("$provider $mode crosses native SSH stdio and closes its owner", async ({ provider, mode }) => {
    const root = await realpath(await mkdtemp(path.join(liveRoot!, "native-transport-")));
    const platform = process.platform === "darwin" ? "mac" : "linux";
    const runtime = path.resolve(`resources/native/${platform}-${process.arch}`);
    // This server is the current account's existing CLI login. The client
    // transport never receives this environment or copies credential files.
    const server = await terminalSshServer(root, getExpandedEnv().PATH);
    Object.assign(server.env, { HOME: homedir(), CRAB_BIN: path.join(runtime, "crab"), TMPDIR: root, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" });
    const client = await server.connect();
    const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
    const command = `printf '%s' "$$" > ${quote(path.join(root, ".sidecar.pid"))}\nexec ${quote(path.join(runtime, "crab-desktop-agent"))}`;
    const channel = await new Promise<ClientChannel>((resolve, reject) => client.exec(command, (error, value) => error ? reject(error) : resolve(value)));
    let diagnostics = "";
    channel.stderr.on("data", (chunk: Buffer) => { diagnostics = (diagnostics + chunk.toString()).slice(-64 * 1024); });
    let exit: number | null = null;
    channel.on("exit", code => { exit = code; });
    const closed = new Promise<number | null>((resolve, reject) => { channel.once("close", () => resolve(exit)); channel.once("error", reject); });
    const manager = { getStatus: () => "connected", getConnection: () => ({ profile: {} }) } as unknown as SshSessionManager;
    const router = new RpcRouter(manager, { request: async () => { throw new Error("Native transport cannot fall through to local RPC"); } });
    router.attachChannel("owned-server", channel as unknown as ClientChannel);
    const factory = remoteAgentProcessFactory(router, "owned-server");
    const createAdapter = () => provider === "codex" ? new CodexAppServerAdapter(undefined, undefined, "codex", factory) : new ClaudeCodeAdapter([], "claude", factory);
    let adapter = createAdapter();
    const events: AgentEvent[] = [];
    let version = "";
    let models: Awaited<ReturnType<typeof adapter.listModels>> = [];
    try {
      const binary = provider === "codex" ? "codex" : "claude";
      const versionProcess = factory(binary, ["--version"], { cwd: root });
      const versionBytes: Buffer[] = [];
      versionProcess.process.on("error", () => {});
      versionProcess.process.stdout?.on("data", (bytes: Buffer) => versionBytes.push(bytes));
      versionProcess.process.stderr?.resume();
      await versionProcess.closed;
      version = Buffer.concat(versionBytes).toString("utf8").trim();
      models = await adapter.listModels(root);
      expect(models.length).toBeGreaterThan(0);
      await adapter.cancel();
      if (mode === "models") {
        const owned = factory("/bin/sh", ["-c", "printf '%s' \"$$\" > .native.pid; exec sleep 120"], { cwd: root });
        owned.process.on("error", () => {});
        owned.process.stdout?.resume(); owned.process.stderr?.resume();
        await new Promise<void>((resolve, reject) => owned.process.stdin!.write(Buffer.alloc(0), error => error ? reject(error) : resolve()));
        let pid = 0;
        await vi.waitFor(async () => { pid = Number(await readFile(path.join(root, ".native.pid"), "utf8")); expect(pid).toBeGreaterThan(0); });
        const sidecar = Number(await readFile(path.join(root, ".sidecar.pid"), "utf8"));
        process.kill(sidecar, provider === "codex" ? "SIGTERM" : "SIGHUP");
        await owned.closed;
        let running = true;
        try { process.kill(pid, 0); } catch (error: unknown) {
          if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
          running = false;
        }
        expect(running).toBe(false);
      }
      if (mode === "turn") {
        await promisify(execFile)("git", ["init", "--template=", "--initial-branch=main", root]);
        adapter = createAdapter();
        const target = path.join(root, "transport-proof.txt");
        adapter.onEvent(event => {
          events.push(event);
          const approval = event.approval;
          if (event.type !== "approval_request" || !approval) return;
          let allowed = false;
          if (approval.kind === "file") {
            if (provider === "claude-code") {
              const input = JSON.parse(approval.detail ?? "{}") as { file_path?: string };
              allowed = typeof input.file_path === "string" && path.resolve(root, input.file_path) === target;
            } else {
              const granted = approval.detail?.split("\n")[0];
              allowed = granted === root || granted === target;
            }
          }
          adapter.respondApproval(approval.requestId, allowed ? "allow_once" : "deny");
        });
        await new Promise<void>((resolve, reject) => {
          const timer = setTimeout(() => { unsubscribe(); reject(new Error("Native SSH turn did not finish")); }, 90_000);
          const unsubscribe = adapter.onEvent(event => {
            if (event.type !== "done") return;
            clearTimeout(timer); unsubscribe();
            if (event.outcome === "completed") resolve();
            else reject(new Error(events.find(entry => entry.type === "error")?.error ?? "Native SSH turn failed"));
          });
          const prompt = "Use a built-in file writing or patch tool to create transport-proof.txt in this workspace containing exactly native-transport-ok followed by a newline. Do not run shell commands, read credentials, contact external services, or use other tools. Then reply Done.";
          void adapter.spawn(prompt, root, undefined, { mode: "instant" }).catch(error => { clearTimeout(timer); unsubscribe(); reject(error); });
        });
        expect(await readFile(target, "utf8")).toBe("native-transport-ok\n");
        await adapter.cancel();
      }
    } finally {
      const cancellation = adapter.cancel();
      const closeInput = setTimeout(() => channel.end(), 5_000);
      const timer = setTimeout(() => channel.destroy(), 15_000);
      void cancellation.finally(() => channel.end()).catch(() => {});
      try {
        const results = await Promise.allSettled([cancellation, closed]);
        await writeFile(path.join(liveRoot!, `${provider}-native-transport-${mode}.json`), JSON.stringify({ version, models, events, diagnostics, shutdown: results.map(result => result.status === "rejected" ? { status: result.status, error: String(result.reason) } : result) }, null, 2));
        if (results[0].status === "rejected") throw results[0].reason;
        if (results[1].status === "rejected") throw results[1].reason;
        expect(results[1].value).toBe(0);
      } finally { clearTimeout(closeInput); clearTimeout(timer); router.detachChannel("owned-server"); client.end(); await server.close(); await rm(root, { recursive: true, force: true }); }
    }
  }, 120_000);
});
