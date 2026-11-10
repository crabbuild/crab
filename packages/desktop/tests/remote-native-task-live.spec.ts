import { describe, expect, it, vi } from "vitest";
import { execFile } from "node:child_process";
import { mkdtemp, mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import { homedir } from "node:os";
import path from "node:path";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { Orchestrator, type Task } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";
import { getExpandedEnv, type AgentEvent } from "../electron/orchestrator-adapters";
import { terminalSshServer } from "./fixtures/ssh-terminal-server";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

describe.skipIf(!liveRoot || process.platform === "win32")("shared remote native task lifecycle", () => {
  it.each(["checkpoints", "codex", "claude-code"] as const)("%s uses the real sidecar and workspace host", async mode => {
    const root = await realpath(await mkdtemp(path.join(liveRoot!, "remote-native-task-")));
    const repo = path.join(root, "repo"); await mkdir(repo);
    await run("git", ["init", "--template=", "--initial-branch=main", repo]);
    await writeFile(path.join(repo, "seed.txt"), "before\n");
    await run("git", ["add", "."], { cwd: repo });
    await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--no-gpg-sign", "-m", "baseline"], { cwd: repo });
    const platform = process.platform === "darwin" ? "mac" : "linux";
    const runtime = path.resolve(`resources/native/${platform}-${process.arch}`);
    const server = await terminalSshServer(root, getExpandedEnv().PATH);
    // Keep temporary worktrees in this fixture; use the server account's
    // existing CLI configuration/login directories without copying credentials.
    Object.assign(server.env, { CRAB_BIN: path.join(runtime, "crab"), TMPDIR: root, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1",
      CODEX_HOME: process.env.CODEX_HOME ?? path.join(homedir(), ".codex"), CLAUDE_CONFIG_DIR: process.env.CLAUDE_CONFIG_DIR ?? path.join(homedir(), ".claude") });
    const client = await server.connect();
    const channel = await new Promise<ClientChannel>((resolve, reject) => client.exec(`exec ${quote(path.join(runtime, "crab-desktop-agent"))}`, (error, stream) => error ? reject(error) : resolve(stream)));
    let diagnostics = ""; channel.stderr.on("data", (bytes: Buffer) => { diagnostics = (diagnostics + bytes.toString()).slice(-64 * 1024); });
    const closed = new Promise<void>((resolve, reject) => { channel.once("close", resolve); channel.once("error", reject); });
    const manager = { getStatus: () => "connected", getConnection: () => ({ profile: {} }) } as unknown as SshSessionManager;
    const router = new RpcRouter(manager, { request: async () => { throw new Error("Remote task cannot use local RPC"); } });
    router.attachChannel("fixture", channel);
    const repoId = `ssh://fixture${repo}`;
    const persistPath = path.join(root, "tasks.sqlite");
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(root, "settings.json"));
    const local = { binaryPath: "/client-only/provider", extraArgs: ["--client-only"], env: { CLIENT_ONLY_CREDENTIAL: "client-scope-only-value" } };
    await writeFile(path.join(root, "settings.json"), JSON.stringify({ orchestratorAgentConfigs: { codex: local, "claude-code": local } }));
    const requests = vi.spyOn(router, "requestOnRemoteChannel");
    let orchestrator = new Orchestrator({ persistPath, rpcRouter: router, timeoutMs: 120_000 });
    const db = new ChatDatabase(path.join(root, "chat.sqlite"));
    const events: AgentEvent[] = [];
    let task: Task | undefined;
    const wire = () => orchestrator.on("orchestrator:event", ({ taskId, event }: { taskId: string; event: AgentEvent }) => {
      events.push(event);
      const approval = event.approval;
      if (!approval || !task) return;
      let allowed = false;
      if (approval.kind === "file") {
        if (mode === "claude-code") {
          const input = JSON.parse(approval.detail ?? "{}") as { file_path?: string };
          allowed = input.file_path === path.join(task.worktreePath, "remote-proof.txt");
        } else allowed = [task.worktreePath, path.join(task.worktreePath, "remote-proof.txt")].includes(approval.detail?.split("\n")[0] ?? "");
      }
      orchestrator.recordApproval(taskId, { requestId: approval.requestId, decision: allowed ? "allow_once" : "deny", summary: "Owned remote fixture" });
    });
    const wait = async () => vi.waitFor(() => {
      if (task?.status === "failed") throw new Error(events.find(event => event.error)?.error ?? "Remote native turn failed");
      expect(task?.status).toBe("reviewing");
    }, { timeout: 120_000, interval: 100 });
    let succeeded = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await router.sendToRemoteAgent("fixture", "open_local", { repo_id: repoId, path: repo });
      task = await orchestrator.createTask("Use a built-in file writing or patch tool to create remote-proof.txt containing exactly remote-one followed by a newline. Do not use shell commands, external services or subagents. Reply Done.", mode === "claude-code" ? mode : "codex", repoId);
      const checkpoints = orchestrator.checkpointManager(task);
      if (mode === "checkpoints") {
        await writeFile(path.join(task.worktreePath, ".gitignore"), "ignored-new.txt\n");
        await writeFile(path.join(task.worktreePath, "ignored-new.txt"), "staged ignored addition\n");
        await run("git", ["add", "-f", "ignored-new.txt"], { cwd: task.worktreePath });
        await writeFile(path.join(task.worktreePath, "seed.txt"), "staged\n");
        await run("git", ["add", "seed.txt"], { cwd: task.worktreePath });
        const index = (await run("git", ["rev-parse", "--git-path", "index"], { cwd: task.worktreePath })).stdout.trim();
        const original = await readFile(path.resolve(task.worktreePath, index));
        await writeFile(path.join(task.worktreePath, "seed.txt"), "unstaged\n");
        await writeFile(path.join(task.worktreePath, "ignored-new.txt"), "updated ignored addition\n");
        await writeFile(path.join(task.worktreePath, "雪🙂.txt"), "untracked\n");
        const captured = await checkpoints.createCheckpoint(task, 0);
        const files = await run("git", ["ls-tree", "-r", "--name-only", captured.sha], { cwd: repo });
        expect({ index: await readFile(path.resolve(task.worktreePath, index)), listed: await checkpoints.listCheckpoints(task), tracked: (await run("git", ["show", `${captured.sha}:seed.txt`], { cwd: repo })).stdout,
          untracked: (await run("git", ["show", `${captured.sha}:雪🙂.txt`], { cwd: repo })).stdout,
          ignored: (await run("git", ["show", `${captured.sha}:ignored-new.txt`], { cwd: repo })).stdout, hasFiles: files.stdout.length > 0 }).toEqual({ index: original, listed: [{ ...captured, turnNumber: 0 }], tracked: "unstaged\n", untracked: "untracked\n", ignored: "updated ignored addition\n", hasFiles: true });
        await checkpoints.holdCheckpoint(task, 1, captured.sha);
        await checkpoints.recordCheckpoint(task, 1, captured.sha); await checkpoints.releaseCheckpoint(task, 1);
        expect((await checkpoints.listCheckpoints(task)).map(checkpoint => checkpoint.turnNumber)).toEqual([0, 1]);
        const lock = path.join(repo, ".git", "refs", "crab", "checkpoints", task.id, "2.lock");
        await writeFile(lock, "owned fixture lock");
        try {
          await expect(checkpoints.createCheckpoint(task, 2)).rejects.toThrow();
          expect((await checkpoints.listCheckpoints(task)).map(checkpoint => checkpoint.turnNumber)).toEqual([0, 1]);
        } finally { await rm(lock); }
      } else {
        wire();
        type Handler = Parameters<Parameters<typeof registerChatIpc>[0]["handle"]>[1];
        const handlers = new Map<string, Handler>();
        let chat = registerChatIpc({ handle: (name, handler) => handlers.set(name, handler) }, orchestrator, db, () => null);
        await orchestrator.startTask(task.id); await wait();
        const session = mode === "codex" ? task.codexThreadId : task.claudeSessionId;
        expect(Boolean(session)).toBe(true);
        await handlers.get("orchestrator:iterate")!({} as Parameters<Handler>[0], { taskId: task.id, feedback: "Use your built-in edit/patch tool to replace remote-one with remote-two in remote-proof.txt. Do not use other tools, shell commands or subagents. Reply Done.", messageId: "remote-warm-input" });
        await wait();
        await orchestrator.shutdown();
        orchestrator = new Orchestrator({ persistPath, rpcRouter: router, timeoutMs: 120_000 });
        task = orchestrator.getTask(task.id)!; wire();
        chat = registerChatIpc({ handle: (name, handler) => handlers.set(name, handler) }, orchestrator, db, () => null);
        await orchestrator.iterate(task.id, "Use your built-in edit/patch tool to replace remote-two with remote-three in remote-proof.txt. Do not use other tools, shell commands or subagents. Reply Done."); await wait();
        expect({ file: await readFile(path.join(task.worktreePath, "remote-proof.txt"), "utf8"), session: mode === "codex" ? task.codexThreadId : task.claudeSessionId,
          turns: (await orchestrator.checkpointManager(task).listCheckpoints(task)).map(checkpoint => checkpoint.turnNumber), feedback: task.feedback.length,
          visible: chat.snapshot(task.id).messages.some(message => message.id === "remote-warm-input") }).toEqual({ file: "remote-three\n", session, turns: [0, 1, 2], feedback: 2, visible: true });
      }
      if (mode === "codex") {
        await orchestrator.applyTask(task.id);
        expect({ status: task.status, file: await readFile(path.join(repo, "remote-proof.txt"), "utf8") }).toEqual({ status: "applied", file: "remote-three\n" });
      } else {
        await orchestrator.discardTask(task.id);
        expect({ status: task.status, checkpoints: await orchestrator.checkpointManager(task).listCheckpoints(task) }).toEqual({ status: "discarded", checkpoints: [] });
      }
      const opens = requests.mock.calls.filter(call => call[2] === "native_process_open").map(call => call[3]);
      expect(opens.length > 0 && opens.every(params => params && typeof params === "object" && !("env" in params) && !JSON.stringify(params).includes("client-only") && !JSON.stringify(params).includes("client-scope-only-value"))).toBe(true);
      succeeded = true;
    } finally {
      await orchestrator.shutdown().catch(error => { diagnostics += `\nShutdown: ${String(error)}`; });
      channel.end(); timer = setTimeout(() => channel.destroy(), 15_000);
      try { await closed; } finally {
        clearTimeout(timer); db.close(); router.detachChannel("fixture"); client.end(); await server.close(); requests.mockRestore(); vi.unstubAllEnvs();
        await writeFile(path.join(liveRoot!, `${mode}-remote-native-task.json`), JSON.stringify({ root, task, events, diagnostics }, null, 2));
        if (succeeded) await rm(root, { recursive: true, force: true });
      }
    }
  }, 420_000);
});
