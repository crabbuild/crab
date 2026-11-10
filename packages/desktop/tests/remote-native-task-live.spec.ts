import { describe, expect, it, vi } from "vitest";
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, mkdir, readFile, realpath, rm, utimes, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import { homedir } from "node:os";
import path from "node:path";
import { EventEmitter } from "node:events";
import type { IpcMain } from "electron";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { Orchestrator, type Task } from "../electron/orchestrator";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";
import { registerModelDiscoveryIpc } from "../electron/orchestrator-models";
import { AgentDetector, registerAgentDetectionIpc } from "../electron/ssh-agent-detection";
import { getExpandedEnv, type AgentEvent, type AgentModel } from "../electron/orchestrator-adapters";
import { terminalSshServer } from "./fixtures/ssh-terminal-server";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

describe.skipIf(!liveRoot || process.platform === "win32")("shared remote native task lifecycle", () => {
  it.each(["checkpoints", "checkpoint-stop-local", "checkpoint-stop-remote", "apply-target", "apply-target-deleted", "apply-target-detached", "apply-eof", "availability", "catalogs", "codex", "claude-code"] as const)("%s uses the real sidecar and workspace host", async mode => {
    const root = await realpath(await mkdtemp(path.join(liveRoot!, "remote-native-task-")));
    const repo = path.join(root, "repo"); await mkdir(repo);
    await run("git", ["init", "--template=", "--initial-branch=main", repo]);
    await writeFile(path.join(repo, "seed.txt"), "before\n");
    await run("git", ["add", "."], { cwd: repo });
    await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--no-gpg-sign", "-m", "baseline"], { cwd: repo });
    const platform = process.platform === "darwin" ? "mac" : "linux";
    const runtime = path.resolve(`resources/native/${platform}-${process.arch}`);
    const server = await terminalSshServer(root, getExpandedEnv().PATH);
    const bin = path.join(root, "bin");
    if (mode === "availability") {
      await mkdir(bin);
      for (const binary of ["claude", "codex"]) await writeFile(path.join(bin, binary), `#!/bin/sh\nprintf '${binary} server-fixture\\n'\n`, { mode: 0o755 });
      server.env.PATH = `${bin}:/usr/bin:/bin`;
    }
    // Keep temporary worktrees in this fixture; use the server account's
    // existing CLI configuration/login directories without copying credentials.
    Object.assign(server.env, { CRAB_BIN: path.join(runtime, "crab"), TMPDIR: root, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1",
      CODEX_HOME: process.env.CODEX_HOME ?? path.join(homedir(), ".codex"), CLAUDE_CONFIG_DIR: process.env.CLAUDE_CONFIG_DIR ?? path.join(homedir(), ".claude") });
    const client = await server.connect();
    const openAgent = () => new Promise<ClientChannel>((resolve, reject) => client.exec(`exec ${quote(path.join(runtime, "crab-desktop-agent"))}`, (error, stream) => error ? reject(error) : resolve(stream)));
    const channel = await openAgent();
    let diagnostics = ""; channel.stderr.on("data", (bytes: Buffer) => { diagnostics = (diagnostics + bytes.toString()).slice(-64 * 1024); });
    const closed = new Promise<void>((resolve, reject) => { channel.once("close", resolve); channel.once("error", reject); });
    const endpoint = { host: "127.0.0.1", port: server.connection.port, username: "fixture",
      hostKey: `SHA256:${Buffer.from(server.connection.fingerprint, "hex").toString("base64").replace(/=+$/, "")}` };
    const manager = { getStatus: () => "connected", getConnection: () => ({ profile: {}, endpoint, channels: { agent: channel } }) } as unknown as SshSessionManager;
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
    const catalogs: Record<string, { repository: number; workspace: number }> = {};
    let availability: unknown;
    let cancellation: unknown;
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
      if (mode === "apply-target-detached") await run("git", ["checkout", "--detach"], { cwd: repo });
      task = await orchestrator.createTask("Use a built-in file writing or patch tool to create remote-proof.txt containing exactly remote-one followed by a newline. Do not use shell commands, external services or subagents. Reply Done.", mode === "claude-code" ? mode : "codex", mode === "checkpoint-stop-local" ? repo : repoId);
      if (mode.startsWith("apply-target")) {
        await orchestrator.shutdown();
        orchestrator = new Orchestrator({ persistPath, rpcRouter: router });
        task = orchestrator.getTask(task.id)!;
        const head = (await run("git", ["rev-parse", "HEAD"], { cwd: repo })).stdout;
        const index = (await run("git", ["rev-parse", "--git-path", "index"], { cwd: task.worktreePath })).stdout.trim();
        const originalIndex = await readFile(path.resolve(task.worktreePath, index));
        await writeFile(path.join(task.worktreePath, "apply-target.txt"), "captured target bytes\n");
        await run("git", ["checkout", "-b", "other"], { cwd: repo });
        if (mode === "apply-target-deleted") await run("git", ["branch", "-D", "main"], { cwd: repo });
        if (mode === "apply-target") {
          await orchestrator.applyTask(task.id);
          expect({ captured: task.baseBranch, active: (await run("git", ["branch", "--show-current"], { cwd: repo })).stdout.trim(),
            target: (await run("git", ["show", "main:apply-target.txt"], { cwd: repo })).stdout,
            activeFiles: (await run("git", ["ls-tree", "--name-only", "HEAD", "apply-target.txt"], { cwd: repo })).stdout.trim(),
            status: task.status, worktree: existsSync(task.worktreePath) }).toEqual({ captured: "main", active: "other", target: "captured target bytes\n", activeFiles: "", status: "applied", worktree: false });
        } else {
          await expect(orchestrator.applyTask(task.id)).rejects.toThrow(/target/i);
          // Apply drains a created conversation before backend validation;
          // failure retains its stopped owner and untouched workspace for recovery.
          expect({ head: (await run("git", ["rev-parse", "HEAD"], { cwd: repo })).stdout,
            source: (await run("git", ["rev-parse", "HEAD"], { cwd: task.worktreePath })).stdout,
            bytes: await readFile(path.join(task.worktreePath, "apply-target.txt"), "utf8"), index: await readFile(path.resolve(task.worktreePath, index)),
            captured: task.baseBranch, status: task.status }).toEqual({ head, source: head, bytes: "captured target bytes\n", index: originalIndex,
            captured: mode === "apply-target-deleted" ? "main" : undefined, status: "stopped" });
          await orchestrator.discardTask(task.id);
        }
      } else if (mode === "apply-eof") {
        const worktree = task.worktreePath;
        const head = (await run("git", ["rev-parse", "HEAD"], { cwd: worktree })).stdout;
        const started = path.join(root, "hook-pid");
        const release = path.join(root, "release-hook");
        const late = path.join(worktree, "late.txt");
        await writeFile(path.join(worktree, "pending.txt"), "pending source bytes\n");
        await mkdir(path.join(repo, ".git", "hooks"), { recursive: true });
        await writeFile(path.join(repo, ".git", "hooks", "pre-commit"), `#!/bin/sh
printf '%s\\n' "$$" > ${quote(started)}
for i in $(seq 1 300); do
  if [ -f ${quote(release)} ]; then
    printf 'late write\\n' > ${quote(late)}
    exit 0
  fi
  sleep 0.1
done
exit 1
`, { mode: 0o755 });
        const apply = orchestrator.applyTask(task.id).then(() => true, () => false);
        let hookPid: number | undefined;
        const hookAlive = () => {
          if (!hookPid) return false;
          try { process.kill(hookPid, 0); return true; }
          catch (error) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; }
        };
        try {
          await vi.waitFor(async () => {
            hookPid = Number((await readFile(started, "utf8")).trim());
            expect(hookPid).toBeGreaterThan(1);
          }, { timeout: 15_000 });
          const discard = { repo_id: repoId, task_id: task.id, branch: task.branch, worktree_path: worktree };
          await expect(router.sendToRemoteAgent("fixture", "task_discard", discard)).rejects.toMatchObject({ data: { kind: "task_workspace_busy" } });
          channel.end();
          await closed;
          expect({ reportedApplied: await apply, writerAlive: hookAlive(),
            source: (await run("git", ["rev-parse", "HEAD"], { cwd: worktree })).stdout,
            target: (await run("git", ["rev-parse", "HEAD"], { cwd: repo })).stdout,
            bytes: await readFile(path.join(worktree, "pending.txt"), "utf8")
          }).toEqual({ reportedApplied: false, writerAlive: false, source: head, target: head, bytes: "pending source bytes\n" });
          const replacement = await openAgent();
          replacement.stderr.resume();
          const replacementClosed = new Promise<void>(resolve => replacement.once("close", resolve));
          try {
            router.attachChannel("fixture", replacement);
            await router.restoreRepositories("fixture");
            await expect(router.sendToRemoteAgent("fixture", "task_discard", discard)).rejects.toMatchObject({ data: { kind: "task_apply_pending" } });
            await expect(router.sendToRemoteAgent("fixture", "native_process_open", {
              process_id: randomUUID(), task_id: task.id, executable: "/bin/true", args: [], cwd: worktree,
            })).rejects.toMatchObject({ data: { kind: "task_apply_pending" } });
          } finally {
            replacement.end();
            await replacementClosed;
          }
        } finally {
          await writeFile(release, "release owned hook if still running\n");
          await vi.waitFor(() => expect(hookAlive()).toBe(false), { timeout: 10_000 });
        }
        expect(existsSync(late)).toBe(false);
      } else if (mode === "checkpoints") {
        const saved = task;
        const factory = remoteAgentProcessFactory(router, "fixture");
        const checkpoints = new CheckpointManager(() => factory);
        await writeFile(path.join(saved.worktreePath, ".gitignore"), "ignored-new.txt\n");
        await writeFile(path.join(saved.worktreePath, "ignored-new.txt"), "staged ignored addition\n");
        await run("git", ["add", "-f", "ignored-new.txt"], { cwd: saved.worktreePath });
        await writeFile(path.join(saved.worktreePath, "seed.txt"), "staged\n");
        await run("git", ["add", "seed.txt"], { cwd: saved.worktreePath });
        const index = (await run("git", ["rev-parse", "--git-path", "index"], { cwd: saved.worktreePath })).stdout.trim();
        const original = await readFile(path.resolve(saved.worktreePath, index));
        await writeFile(path.join(saved.worktreePath, "seed.txt"), "unstaged\n");
        await writeFile(path.join(saved.worktreePath, "ignored-new.txt"), "updated ignored addition\n");
        await writeFile(path.join(saved.worktreePath, "雪🙂.txt"), "untracked\n");
        const captured = await checkpoints.createCheckpoint(saved, 0);
        const files = await run("git", ["ls-tree", "-r", "--name-only", captured.sha], { cwd: repo });
        expect({ index: await readFile(path.resolve(saved.worktreePath, index)), listed: await checkpoints.listCheckpoints(saved), tracked: (await run("git", ["show", `${captured.sha}:seed.txt`], { cwd: repo })).stdout,
          untracked: (await run("git", ["show", `${captured.sha}:雪🙂.txt`], { cwd: repo })).stdout,
          ignored: (await run("git", ["show", `${captured.sha}:ignored-new.txt`], { cwd: repo })).stdout, hasFiles: files.stdout.length > 0 }).toEqual({ index: original, listed: [{ ...captured, turnNumber: 0 }], tracked: "unstaged\n", untracked: "untracked\n", ignored: "updated ignored addition\n", hasFiles: true });
        await checkpoints.holdCheckpoint(saved, 1, captured.sha);
        await checkpoints.recordCheckpoint(saved, 1, captured.sha); await checkpoints.releaseCheckpoint(saved, 1);
        expect((await checkpoints.listCheckpoints(saved)).map(checkpoint => checkpoint.turnNumber)).toEqual([0, 1]);
        const lock = path.join(repo, ".git", "refs", "crab", "checkpoints", saved.id, "2.lock");
        await writeFile(lock, "owned fixture lock");
        try {
          await expect(checkpoints.createCheckpoint(saved, 2)).rejects.toThrow();
          expect((await checkpoints.listCheckpoints(saved)).map(checkpoint => checkpoint.turnNumber)).toEqual([0, 1]);
        } finally { await rm(lock); }
      } else if (mode.startsWith("checkpoint-stop-")) {
        const marker = path.join(root, "filter-owner"); const proof = path.join(root, "filter-writes"); const release = path.join(root, "filter-release");
        const script = path.join(root, "filter.sh");
        await writeFile(script, `#!/bin/sh
if [ -z "\${GIT_INDEX_FILE:-}" ]; then exec cat; fi
trap '' TERM
(trap '' TERM; while [ ! -f ${quote(release)} ]; do printf x >> ${quote(proof)}; sleep 0.05; done) &
child=$!
printf '%s\\n' "$$" "$child" "$GIT_INDEX_FILE" > ${quote(marker)}
wait "$child"
`, { mode: 0o755 });
        await run("git", ["config", "filter.crab-checkpoint.clean", `sh ${quote(script)}`], { cwd: repo });
        await run("git", ["config", "filter.crab-checkpoint.required", "true"], { cwd: repo });
        await writeFile(path.join(task.worktreePath, ".gitattributes"), "seed.txt filter=crab-checkpoint\n");
        // A stale stat cache makes status want to refresh the real index even
        // though these file bytes are unchanged. Preparation must leave it alone.
        const modified = new Date(Date.now() + 5000);
        await utimes(path.join(task.worktreePath, "seed.txt"), modified, modified);
        const indexPath = (await run("git", ["rev-parse", "--git-path", "index"], { cwd: task.worktreePath })).stdout.trim();
        const index = path.resolve(task.worktreePath, indexPath); const original = await readFile(index);
        wire();
        const startup = orchestrator.startTask(task.id);
        const cancelled = expect(startup).rejects.toThrow(/cancelled/);
        let stopping: Promise<void> | undefined;
        let deadline: ReturnType<typeof setTimeout> | undefined;
        try {
          await vi.waitFor(async () => expect((await readFile(marker, "utf8")).trim().split("\n")).toHaveLength(3), { timeout: 15_000 });
          const [parent, child, privateIndex] = (await readFile(marker, "utf8")).trim().split("\n");
          stopping = orchestrator.cancelTask(task.id);
          await Promise.race([stopping, new Promise<never>((_, reject) => { deadline = setTimeout(() => reject(new Error("Checkpoint Stop did not close its writer")), 20_000); })]);
          const alive = (pid: string) => { try { process.kill(Number(pid), 0); return true; } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; return false; } };
          const bytes = await readFile(proof);
          await new Promise(resolve => setTimeout(resolve, 200));
          expect({ status: task.status, writers: [alive(parent), alive(child)], proof: await readFile(proof), index: await readFile(index), events }).toEqual({ status: "stopped", writers: [false, false], proof: bytes, index: original, events: [] });
          await expect(readFile(privateIndex)).rejects.toMatchObject({ code: "ENOENT" });
          await expect(readFile(`${privateIndex}.lock`)).rejects.toMatchObject({ code: "ENOENT" });
          cancellation = { host: task.isRemote ? "ssh" : "local", pids: [Number(parent), Number(child)], writersStopped: true, userIndexUnchanged: true, privateIndexRemoved: true, privateLockRemoved: true, outputStable: true, providerEvents: events.length };
          await cancelled;
        } finally {
          clearTimeout(deadline); await writeFile(release, "release fixture writer\n");
          await startup.catch(() => {}); await stopping?.catch(() => {});
          await run("git", ["config", "--remove-section", "filter.crab-checkpoint"], { cwd: repo });
        }
      } else if (mode === "availability") {
        type Handler = Parameters<IpcMain["handle"]>[1];
        const handlers = new Map<string, Handler>();
        const stop = registerAgentDetectionIpc({ handle: (name: string, handler: Handler) => handlers.set(name, handler) } as unknown as IpcMain, () => router);
        const detect = () => handlers.get("orchestrator:detect-agents")!({} as Parameters<Handler>[0], { repoId });
        try {
          availability = await detect();
          expect(availability).toEqual({ success: true, agents: [
            { type: "claude-code", name: "Claude Code", binary: "claude", path: path.join(bin, "claude"), version: "claude server-fixture", available: true },
            { type: "codex", name: "Codex", binary: "codex", path: path.join(bin, "codex"), version: "codex server-fixture", available: true },
          ] });
          await rm(path.join(bin, "codex"));
          expect(await detect()).toMatchObject({ success: true, agents: [{ available: true }, { available: false, path: "" }] });
          await writeFile(path.join(bin, "claude"), "#!/bin/sh\nprintf 'Broken server executable' >&2\nexit 7\n", { mode: 0o755 });
          expect(await detect()).toMatchObject({ success: false, error: expect.stringContaining("Broken server executable") });
          expect(requests.mock.calls.some(call => ["detect_agents", "exec_command"].includes(call[2]))).toBe(false);
        } finally { await stop(); }
      } else if (mode === "catalogs") {
        const detector = new AgentDetector(() => router);
        try {
          availability = await detector.detect(repoId);
          expect(availability).toEqual(expect.arrayContaining([
            expect.objectContaining({ type: "codex", available: true }), expect.objectContaining({ type: "claude-code", available: true }),
          ]));
        } finally { await detector.shutdown(); }
        type Handler = Parameters<IpcMain["handle"]>[1];
        const handlers = new Map<string, Handler>();
        const stop = registerModelDiscoveryIpc({ handle: (name: string, handler: Handler) => handlers.set(name, handler) } as unknown as IpcMain, orchestrator);
        const sender = Object.assign(new EventEmitter(), { id: 1 });
        const discover = (agentType: string, cwd: string, taskId?: string) => handlers.get("orchestrator:list-models")!(
          { sender } as Parameters<Handler>[0], { agentType, cwd, taskId, requestId: crypto.randomUUID() },
        ) as Promise<{ success: boolean; models?: AgentModel[]; error?: string }>;
        try {
          for (const provider of ["codex", "claude-code"]) {
            const repository = await discover(provider, repoId);
            // A caller's stale raw path must not override saved SSH ownership.
            const workspace = await discover(provider, "/client-only/stale-path", task.id);
            expect([repository, workspace]).toEqual([{ success: true, models: expect.any(Array) }, { success: true, models: expect.any(Array) }]);
            catalogs[provider] = { repository: repository.models!.length, workspace: workspace.models!.length };
            expect(Object.values(catalogs[provider]).every(count => count > 0)).toBe(true);
          }
          const opens = requests.mock.calls.filter(call => call[2] === "native_process_open").map(call => call[3]);
          const contexts = opens.map(params => ({ cwd: (params as { cwd: string }).cwd, taskId: (params as { task_id?: string }).task_id }));
          expect(contexts.slice(-4)).toEqual([repo, task.worktreePath, repo, task.worktreePath].map(cwd => ({ cwd, taskId: cwd === repo ? undefined : task!.id })));
          expect(events).toEqual([]);
        } finally { await stop(); }
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
          turns: (await orchestrator.queryCheckpoints(task, manager => manager.listCheckpoints(task!))).map(checkpoint => checkpoint.turnNumber), feedback: task.feedback.length,
          visible: chat.snapshot(task.id).messages.some(message => message.id === "remote-warm-input") }).toEqual({ file: "remote-three\n", session, turns: [0, 1, 2], feedback: 2, visible: true });
      }
      if (mode === "codex") {
        await orchestrator.applyTask(task.id);
        expect({ status: task.status, file: await readFile(path.join(repo, "remote-proof.txt"), "utf8") }).toEqual({ status: "applied", file: "remote-three\n" });
      } else if (!mode.startsWith("apply-")) {
        await orchestrator.discardTask(task.id);
        expect({ status: task.status, checkpoints: await orchestrator.queryCheckpoints(task, manager => manager.listCheckpoints(task!)) }).toEqual({ status: "discarded", checkpoints: [] });
      }
      const opens = requests.mock.calls.filter(call => call[2] === "native_process_open").map(call => call[3]);
      if (mode === "checkpoint-stop-local" || mode === "apply-target" || mode === "apply-eof") expect(opens).toEqual([]);
      else expect(opens.length > 0 && opens.every(params => params && typeof params === "object" && !("env" in params) && !JSON.stringify(params).includes("client-only") && !JSON.stringify(params).includes("client-scope-only-value"))).toBe(true);
      succeeded = true;
    } finally {
      await orchestrator.shutdown().catch(error => { diagnostics += `\nShutdown: ${String(error)}`; });
      channel.end(); timer = setTimeout(() => channel.destroy(), 15_000);
      try { await closed; } finally {
        clearTimeout(timer); db.close(); router.detachChannel("fixture"); client.end(); await server.close(); requests.mockRestore(); vi.unstubAllEnvs();
        await writeFile(path.join(liveRoot!, `${mode}-remote-native-task.json`), JSON.stringify({ root, task, catalogs, availability, cancellation, events, diagnostics }, null, 2));
        if (succeeded) await rm(root, { recursive: true, force: true });
      }
    }
  }, 420_000);
});
