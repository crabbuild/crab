import { spawn, type ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { readSettings } from "./orchestrator-settings";
import type { Task } from "./orchestrator";

export interface OrchestratorScripts {
  setupScript: string;
  runScript: string;
  archiveScript: string;
}

export interface ScriptResult {
  success: boolean;
  output: string;
  exitCode: number;
  error?: string;
}

export type WorkspaceScriptOptions = Partial<OrchestratorScripts> & {
  command?: string;
  portRangeStart?: number;
  portRangeEnd?: number;
};

export type WorkspaceScriptEvent =
  | { type: "data"; taskId: string; kind: "setup" | "run" | "archive"; command: string; data: string }
  | { type: "exit"; taskId: string; exitCode: number }
  | { type: "nonconcurrent-stop"; stoppedTaskId: string; startedTaskId: string };

export class WorkspaceScriptCancelledError extends Error {
  constructor() { super("Workspace script cancelled"); }
}

type ScriptTask = Pick<Task, "id" | "repoPath" | "worktreePath" | "branch">;
type Configuration = { scripts?: { setup?: string; run?: string; archive?: string }; portRange?: { start?: number; end?: number }; nonconcurrentRun?: boolean };
type Request = { taskId: string; kind: Script["kind"]; controller: AbortController; settled: Promise<void> };
type Workspace = { port: number; sealed: boolean; scripts: Set<Script>; requests: Set<Request>; run?: Script };
type Script = { taskId: string; kind: "setup" | "run" | "archive"; child: ChildProcess; finished: Promise<ScriptResult>; stopping?: Promise<void>; cancelled: boolean };

function configuration(task: ScriptTask, options: WorkspaceScriptOptions) {
  const settings = readSettings();
  let config: Configuration = {};
  // The tagged repository config contract tolerates missing/malformed files
  // and uses saved desktop settings for fields the repository does not set.
  try {
    const saved: unknown = JSON.parse(fs.readFileSync(path.join(task.repoPath, "crab-orchestrator.json"), "utf8"));
    if (saved && typeof saved === "object" && !Array.isArray(saved)) config = saved as Configuration;
  } catch {}
  const text = (key: string) => typeof settings[key] === "string" ? settings[key] as string : "";
  const number = (key: string, fallback: number) => typeof settings[key] === "number" ? settings[key] as number : fallback;
  return {
    setup: config.scripts?.setup ?? options.setupScript ?? text("orchestratorSetupScript"),
    run: options.command ?? config.scripts?.run ?? options.runScript ?? text("orchestratorRunScript"),
    archive: config.scripts?.archive ?? options.archiveScript ?? text("orchestratorArchiveScript"),
    start: options.portRangeStart ?? config.portRange?.start ?? number("orchestratorPortRangeStart", 3100),
    end: options.portRangeEnd ?? config.portRange?.end ?? number("orchestratorPortRangeEnd", 3199),
    nonconcurrent: config.nonconcurrentRun ?? (settings.orchestratorNonconcurrentRun === true),
  };
}

/** Owns local script groups and workspace ports through their actual closure. */
export class WorkspaceScripts {
  private workspaces = new Map<string, Workspace>();
  private runQueue: Promise<unknown> = Promise.resolve();
  private shutdownPromise?: Promise<void>;

  constructor(private readonly emit: (event: WorkspaceScriptEvent) => void) {}

  async setup(task: ScriptTask, options: WorkspaceScriptOptions, signal?: AbortSignal): Promise<ScriptResult & { skipped?: boolean }> {
    const config = configuration(task, options);
    if (!config.setup.trim()) return { success: true, output: "", exitCode: 0, skipped: true };
    const workspace = this.workspace(task, config);
    return this.request(workspace, task.id, "setup", async requestSignal => {
      requestSignal.throwIfAborted();
      signal?.throwIfAborted();
      const script = this.spawn(workspace, task, "setup", config.setup);
      let abort!: () => void;
      const stopped = new Promise<never>((_resolve, reject) => {
        abort = () => { void this.stopScript(script).catch(error => { workspace.sealed = true; reject(error); }); };
      });
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) abort();
      try { return await Promise.race([script.finished, stopped]); }
      finally { signal?.removeEventListener("abort", abort); }
    });
  }

  async run(task: ScriptTask, options: WorkspaceScriptOptions): Promise<{ port: number; stoppedTaskId: string | null }> {
    const config = configuration(task, options);
    if (!config.run.trim()) throw new Error("No run script configured");
    const workspace = this.workspace(task, config);
    return this.request(workspace, task.id, "run", signal => {
      // Serialize replacement and nonconcurrent admission across callers. A
      // late close callback must never remove the next process's ownership.
      const queued = this.runQueue.then(async () => {
        signal.throwIfAborted();
        const previous = config.nonconcurrent
          ? [...this.workspaces.values()].flatMap(entry => entry.run && entry !== workspace ? [entry.run] : []) : [];
        await Promise.all(previous.map(script => this.stopScript(script)));
        if (workspace.run) await this.stopScript(workspace.run);
        signal.throwIfAborted();
        if (workspace.sealed || this.shutdownPromise) throw new Error("Workspace scripts are closing");
        const script = this.spawn(workspace, task, "run", config.run);
        workspace.run = script;
        await new Promise<void>((resolve, reject) => { script.child.once("spawn", resolve); script.child.once("error", reject); });
        for (const stopped of previous) this.emit({ type: "nonconcurrent-stop", stoppedTaskId: stopped.taskId, startedTaskId: task.id });
        return { port: workspace.port, stoppedTaskId: previous.at(-1)?.taskId ?? null };
      });
      this.runQueue = queued.catch(() => {});
      return queued;
    });
  }

  async archive(task: ScriptTask, options: WorkspaceScriptOptions): Promise<void> {
    const config = configuration(task, options);
    if (!config.archive.trim() || !fs.existsSync(task.worktreePath)) return;
    const workspace = this.workspace(task, config);
    const result = await this.request(workspace, task.id, "archive", signal => {
      signal.throwIfAborted();
      const script = this.spawn(workspace, task, "archive", config.archive);
      return script.finished.then(result => {
        if (script.cancelled) throw new WorkspaceScriptCancelledError();
        return result;
      });
    });
    if (!result.success) throw new Error(result.error ?? "Archive script failed");
  }

  async close(task: ScriptTask, wholeWorkspace: boolean): Promise<void> {
    const key = path.resolve(task.worktreePath);
    const workspace = this.workspaces.get(key);
    if (!workspace) return;
    if (wholeWorkspace) workspace.sealed = true;
    // Preparation is shared by conversations. Its orchestrator subscribers
    // decide cancellation; closing a borrower cannot stop a peer's setup.
    const requests = [...workspace.requests].filter(request => wholeWorkspace || request.taskId === task.id && request.kind !== "setup");
    for (const request of requests) request.controller.abort(new WorkspaceScriptCancelledError());
    const scripts = [...workspace.scripts].filter(script => wholeWorkspace || script.taskId === task.id && script.kind !== "setup");
    const results = await Promise.allSettled([...scripts.map(script => this.stopScript(script)), ...requests.map(request => request.settled)]);
    const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
    if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Workspace scripts did not close");
    if (wholeWorkspace) workspace.sealed = false;
  }

  release(task: ScriptTask): void {
    const key = path.resolve(task.worktreePath);
    const workspace = this.workspaces.get(key);
    if (workspace && !workspace.scripts.size && !workspace.requests.size) this.workspaces.delete(key);
  }

  shutdown(): Promise<void> {
    this.shutdownPromise ??= Promise.resolve().then(async () => {
      const results = await Promise.allSettled([...this.workspaces.keys()].map(worktreePath => this.close({ id: "", worktreePath, repoPath: "", branch: "" }, true)));
      const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Workspace script shutdown failed");
      this.workspaces.clear();
    });
    return this.shutdownPromise;
  }

  private workspace(task: ScriptTask, config: ReturnType<typeof configuration>): Workspace {
    if (this.shutdownPromise) throw new WorkspaceScriptCancelledError();
    const key = path.resolve(task.worktreePath);
    const existing = this.workspaces.get(key);
    if (existing?.sealed) throw new Error("Workspace scripts are closing");
    if (existing) return existing;
    if (!Number.isInteger(config.start) || !Number.isInteger(config.end) || config.start < 1 || config.end > 65535 || config.end < config.start) throw new Error("Invalid workspace port range");
    const used = new Set([...this.workspaces.values()].map(workspace => workspace.port));
    let port = config.start;
    while (used.has(port) && port <= config.end) port++;
    if (port > config.end) throw new Error("No workspace port available in the configured range");
    const workspace: Workspace = { port, sealed: false, scripts: new Set(), requests: new Set() };
    this.workspaces.set(key, workspace);
    return workspace;
  }

  private async request<T>(workspace: Workspace, taskId: string, kind: Script["kind"], operation: (signal: AbortSignal) => Promise<T>): Promise<T> {
    let settle!: () => void;
    const request = { taskId, kind, controller: new AbortController(), settled: new Promise<void>(resolve => { settle = resolve; }) };
    workspace.requests.add(request);
    try { return await operation(request.controller.signal); } finally { workspace.requests.delete(request); settle(); }
  }

  private spawn(workspace: Workspace, task: ScriptTask, kind: Script["kind"], command: string): Script {
    const child = spawn(command, { cwd: task.worktreePath, shell: true, detached: process.platform !== "win32", windowsHide: true,
      env: { ...process.env, CRAB_WORKSPACE_PATH: task.worktreePath, CRAB_WORKSPACE_BRANCH: task.branch, CRAB_ROOT_PATH: task.repoPath, CRAB_PORT: String(workspace.port) }, stdio: ["pipe", "pipe", "pipe"] });
    let output = "";
    let error: Error | undefined;
    const closed = new Promise<ScriptResult>(resolve => {
      child.once("error", failure => { error = failure; });
      child.once("close", code => {
        const exitCode = code ?? -1;
        resolve({ success: !error && exitCode === 0, output, exitCode, error: error?.message ?? (exitCode ? `Script exited with code ${exitCode}` : undefined) });
      });
    });
    for (const stream of [child.stdout, child.stderr]) stream?.on("data", (chunk: Buffer) => {
      const data = chunk.toString();
      if (kind !== "run") output += data;
      this.emit({ type: "data", taskId: task.id, kind, command, data });
    });
    const finished = closed.then(async result => {
      // The shell can close while a background descendant still writes with
      // ignored stdio. Retain group ownership until those descendants exit too.
      if (process.platform !== "win32" && child.pid) await this.waitForGroup(child.pid);
      return script.cancelled ? { ...result, success: false, error: "Workspace script cancelled" } : result;
    }).then(result => {
      workspace.scripts.delete(script);
      if (workspace.run === script) workspace.run = undefined;
      return result;
    });
    const script: Script = { taskId: task.id, kind, child, finished, cancelled: false };
    workspace.scripts.add(script);
    void script.finished.then(result => { if (kind === "run") this.emit({ type: "exit", taskId: task.id, exitCode: result.exitCode }); }, error => console.error("[workspace-script] completion failed:", error));
    return script;
  }

  private stopScript(script: Script): Promise<void> {
    if (script.stopping) return script.stopping;
    script.cancelled = true;
    const stopping = (async () => {
      const pid = script.child.pid;
      if (!pid) { await script.finished; return; }
      if (process.platform === "win32") {
        await new Promise<void>((resolve, reject) => {
          const killer = spawn("taskkill", ["/PID", String(pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" });
          killer.once("error", reject);
          killer.once("close", code => { if (code === 0) resolve(); else reject(new Error(`Could not terminate workspace process tree: ${code}`)); });
        });
        await script.finished;
        return;
      }
      this.signalGroup(pid, "SIGTERM");
      let timer: ReturnType<typeof setTimeout> | undefined;
      const escalation = new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => { try { this.signalGroup(pid, "SIGKILL"); } catch (error: unknown) { reject(error); } }, 2000);
      });
      try { await Promise.race([script.finished, escalation]); } finally { clearTimeout(timer); }
    })();
    script.stopping = stopping;
    void stopping.catch(() => { if (script.stopping === stopping) script.stopping = undefined; });
    return stopping;
  }

  private signalGroup(pid: number, signal: NodeJS.Signals): void {
    try { process.kill(-pid, signal); } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; }
  }

  private async waitForGroup(pid: number): Promise<void> {
    while (true) {
      try { process.kill(-pid, 0); } catch (error: unknown) {
        const code = (error as NodeJS.ErrnoException).code;
        if (code === "ESRCH") return;
        // Darwin can report EPERM for a group containing only exiting members.
        // Keep ownership until ESRCH; a denied probe does not prove absence.
        if (code !== "EPERM") throw error;
      }
      await delay(25);
    }
  }
}
