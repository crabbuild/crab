import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";
import * as readline from "node:readline";
import * as path from "node:path";
import * as fs from "node:fs";
import { app, dialog } from "electron";
import type { StorageConnections } from "./storage-connections";
import { parseAgentCapabilities, type AgentCapabilities } from "./ssh-types";

interface RpcSuccess { jsonrpc: "2.0"; id: number | string | null; result: unknown }
interface RpcError { jsonrpc: "2.0"; id: number | string | null; error: { code: number; message: string; data?: unknown } }
interface RpcNotification { jsonrpc: "2.0"; method: string; params: unknown }
type RpcFrame = RpcSuccess | RpcError | RpcNotification;
interface Pending { resolve: (value: unknown) => void; reject: (error: Error) => void }
interface Runtime {
  child: ChildProcessWithoutNullStreams;
  id: string;
  pending: Map<string, Pending>;
  ready: Promise<void>;
  started: Promise<void>;
  closed: Promise<void>;
  capabilities: AgentCapabilities | null;
  failure?: Error;
  stopping: boolean;
}
interface OpenRepository {
  method: "open_local" | "open_url";
  params: Record<string, unknown>;
  sessionId: string | null;
  error?: Error;
}
export interface RepositoryRuntime {
  repoId: string;
  sessionId: string | null;
  error?: string;
}

function fields(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function opened(value: unknown, repoId: string): Record<string, unknown> {
  const result = fields(value);
  if (result?.repo_id !== repoId) throw new Error("Native agent returned an invalid repository identity");
  return result;
}

export class Agent extends EventEmitter {
  private runtime: Runtime | null = null;
  private repositories = new Map<string, OpenRepository>();
  private repositoryWork = new Map<string, Promise<unknown>>();
  private nextId = 1;
  private nextRuntime = 1;
  private nextRepositorySession = 1;
  private stopping = false;
  private crashTimestamps: number[] = [];
  private recentStderrLines: string[] = [];
  private static readonly CRASH_WINDOW_MS = 30_000;
  private static readonly MAX_AUTO_RESTARTS = 1;

  constructor(private readonly binary: string, private readonly env: NodeJS.ProcessEnv = process.env, private readonly extraArgs: string[] = [], private readonly storageConnections?: StorageConnections) { super(); }

  async start(): Promise<void> {
    const existing = this.runtime;
    if (existing) {
      if (!existing.failure && !existing.stopping) return existing.started;
      await existing.closed;
      if (this.stopping || existing.stopping) throw new Error("Native agent stopped during startup");
      return this.start();
    }
    this.stopping = false;
    const child = spawn(this.binary, this.extraArgs, { stdio: ["pipe", "pipe", "pipe"], env: this.env });
    let closed!: () => void;
    const run: Runtime = {
      child, id: `local:${this.nextRuntime++}`, pending: new Map(), stopping: false, capabilities: null,
      closed: new Promise<void>(resolve => { closed = resolve; }),
      ready: Promise.resolve().then(() => this.prepare(run)), started: Promise.resolve(),
    };
    run.started = run.ready.then(() => this.restoreRepositories(run)).catch(async error => {
      if (this.runtime === run) for (const [repoId, repository] of this.repositories) {
        repository.sessionId = null;
        repository.error = error instanceof Error ? error : new Error(String(error));
        this.publishRuntime(repoId, repository);
      }
      await this.stop(run); throw error;
    });
    this.runtime = run;
    child.on("error", error => this.fail(run, error));
    child.stdin.on("error", error => this.fail(run, error));
    readline.createInterface({ input: child.stdout }).on("line", line => { if (line) this.onLine(run, line); });
    readline.createInterface({ input: child.stderr }).on("line", rawLine => {
      const line = this.storageConnections?.redactText(rawLine) ?? rawLine;
      this.recentStderrLines.push(line);
      if (this.recentStderrLines.length > 500) this.recentStderrLines.shift();
      process.stderr.write(`[agent] ${line}\n`);
      this.emit("stderr", line);
    });
    let interrupted: string[] = [];
    child.once("exit", (code, signal) => {
      if (this.runtime !== run) return;
      interrupted = [...run.pending.keys()];
      this.fail(run, new Error(`Native agent exited (code=${code}, signal=${signal ?? "none"})`));
      this.emit("exit", code, signal);
    });
    child.once("close", (code, signal) => {
      closed();
      if (this.runtime !== run) return;
      this.runtime = null;
      // Exit can precede the end of inherited pipes. A replacement must not
      // share pending replies or contend with a process still draining them.
      if (!this.stopping && !run.stopping && run.capabilities) void this.handleCrash(code, signal, interrupted);
    });
    return run.started;
  }

  supports(...methods: string[]): boolean {
    const run = this.runtime;
    const capabilities = run?.capabilities;
    return !!run && !run.failure && !run.stopping && !!capabilities && methods.length > 0 && methods.every(method => capabilities.methods.includes(method));
  }

  repositorySession(repoId: string): string | null {
    const run = this.runtime;
    return run && !run.failure && !run.stopping ? this.repositories.get(repoId)?.sessionId ?? null : null;
  }

  hasRepository(repoId: string): boolean { return this.repositories.has(repoId); }

  private async prepare(run: Runtime): Promise<void> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        (async () => {
          await this.dispatch(run, "ping", {});
          const capabilities = parseAgentCapabilities(await this.dispatch(run, "capabilities", {}));
          this.assertCurrent(run);
          run.capabilities = capabilities;
        })(),
        new Promise<never>((_resolve, reject) => { timer = setTimeout(() => reject(new Error("Native agent startup timed out after 10 s")), 10_000); }),
      ]);
    } finally { clearTimeout(timer); }
  }

  private async restoreRepositories(run: Runtime): Promise<void> {
    await Promise.allSettled([...this.repositories].map(([repoId, repository]) => this.serialize(repoId, async () => {
      if (this.repositories.get(repoId) !== repository) return;
      try {
        const result = this.storageConnections
          ? await this.storageConnections.reopen(repoId, (method, params) => this.sendRequest(run, method, params))
          : await this.sendRequest(run, repository.method, repository.params);
        opened(result, repoId); this.assertCurrent(run);
        if (this.repositories.get(repoId) !== repository) return;
        repository.sessionId = `${run.id}:${this.nextRepositorySession++}`; repository.error = undefined;
        this.publishRuntime(repoId, repository);
      } catch (error) {
        if (this.runtime !== run || run.failure || this.repositories.get(repoId) !== repository) return;
        repository.error = error instanceof Error ? error : new Error(String(error));
        repository.sessionId = null;
        this.publishRuntime(repoId, repository);
        this.emit("notification", "repo/reopen_failed", { repo_id: repoId, message: repository.error.message });
      }
    })));
  }

  async request(method: string, params: unknown): Promise<unknown> {
    const payload = fields(params);
    const repoId = typeof payload?.repo_id === "string" ? payload.repo_id : null;
    if ((method === "open_local" || method === "open_url") && (!this.runtime || this.runtime.failure || this.runtime.stopping)) await this.start();
    if (method === "close" && repoId) this.repositories.delete(repoId);
    const run = this.runtime;
    // A revoked process retains ownership of draining its native repositories.
    // Revoke the desired open even when there is no transport to send close on.
    if (method === "close" && (!run || run.failure || run.stopping)) return {};
    if (!run) throw new Error("Native agent is not running");
    await run.ready;
    this.assertCurrent(run);
    if (repoId && payload && (method === "open_local" || method === "open_url")) {
      return this.serialize(repoId, async () => {
        const previous = this.repositories.get(repoId);
        // Native open removes the old repository before it may fail. Revoke
        // captures before replacement, even when the process stays alive.
        if (previous) { previous.sessionId = null; previous.error = undefined; this.publishRuntime(repoId, previous); }
        try {
          const result = opened(await this.dispatch(run, method, params), repoId);
          this.assertCurrent(run);
          const saved = { ...payload }; delete saved.__storage_auth;
          const repository: OpenRepository = { method, params: saved, sessionId: `${run.id}:${this.nextRepositorySession++}` };
          this.repositories.set(repoId, repository);
          this.publishRuntime(repoId, repository);
          return { ...result, native_session_id: repository.sessionId };
        } catch (error) {
          if (previous && this.runtime === run && !run.failure && this.repositories.get(repoId) === previous) {
            previous.error = error instanceof Error ? error : new Error(String(error));
            this.publishRuntime(repoId, previous);
          }
          throw error;
        }
      });
    }
    if (method === "close" && repoId) return this.serialize(repoId, async () => {
      this.repositories.delete(repoId);
      return this.dispatch(run, method, params);
    });
    if (repoId && method !== "rpc/cancel") {
      // Cancellation must bypass repository setup; waiting for the operation
      // being cancelled would deadlock its control request.
      await this.repositoryWork.get(repoId);
      const failure = this.repositories.get(repoId)?.error;
      if (failure) throw failure;
    }
    return this.dispatch(run, method, params);
  }

  private serialize<T>(repoId: string, operation: () => Promise<T>): Promise<T> {
    const previous = this.repositoryWork.get(repoId);
    const work = (previous ? previous.catch(() => {}) : Promise.resolve()).then(operation);
    this.repositoryWork.set(repoId, work);
    const finished = () => { if (this.repositoryWork.get(repoId) === work) this.repositoryWork.delete(repoId); };
    void work.then(finished, finished);
    return work;
  }

  private dispatch(run: Runtime, method: string, params: unknown): Promise<unknown> {
    return this.storageConnections
      ? this.storageConnections.request(method, params, (name, payload) => this.sendRequest(run, name, payload))
      : this.sendRequest(run, method, params);
  }

  private assertCurrent(run: Runtime): void {
    if (run.failure) throw run.failure;
    if (this.runtime !== run || run.stopping) throw new Error("Native agent session is no longer available");
  }

  private async sendRequest(run: Runtime, method: string, params: unknown): Promise<unknown> {
    this.assertCurrent(run);
    const id = String(this.nextId++);
    const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      run.pending.set(id, { resolve, reject });
      run.child.stdin.write(frame, error => { if (error) { run.pending.delete(id); reject(error); } });
    });
  }

  private publishRuntime(repoId: string, repository: OpenRepository): void {
    this.emit("repository-runtime", { repoId, sessionId: repository.sessionId, error: repository.error?.message } satisfies RepositoryRuntime);
  }

  private fail(run: Runtime, error: Error): void {
    if (this.runtime !== run || run.failure) return;
    run.failure = error;
    for (const pending of run.pending.values()) pending.reject(error);
    run.pending.clear();
    for (const [repoId, repository] of this.repositories) {
      repository.sessionId = null;
      this.publishRuntime(repoId, repository);
    }
  }

  private async stop(run: Runtime): Promise<void> {
    run.stopping = true;
    this.fail(run, new Error("Native agent stopped"));
    run.child.stdin.end();
    // EOF lets the native owner cancel and drain work. A fixed kill timer can
    // truncate publication or close SlateDB while its writer still owns work.
    await run.closed;
  }

  async shutdown(): Promise<void> {
    this.stopping = true;
    if (this.runtime) await this.stop(this.runtime);
  }

  private onLine(run: Runtime, line: string): void {
    if (this.runtime !== run || run.failure) return;
    let frame: RpcFrame;
    try { frame = JSON.parse(line) as RpcFrame; }
    catch { process.stderr.write("[agent] malformed RPC frame\n"); return; }
    if ("id" in frame) {
      const pending = run.pending.get(String(frame.id));
      if (!pending) return;
      run.pending.delete(String(frame.id));
      if ("error" in frame) pending.reject(Object.assign(new Error(frame.error.message), { code: frame.error.code, data: frame.error.data }));
      else pending.resolve(frame.result);
      return;
    }
    if (frame.method === "progress/line" && frame.params && typeof frame.params === "object" && "line" in frame.params && typeof frame.params.line === "string") frame.params.line = this.storageConnections?.redactText(frame.params.line) ?? frame.params.line;
    this.emit("notification", frame.method, frame.params);
  }

  private async handleCrash(code: number | null, signal: string | null, interrupted: string[]): Promise<void> {
    const now = Date.now();
    this.crashTimestamps = this.crashTimestamps.filter(timestamp => now - timestamp < Agent.CRASH_WINDOW_MS);
    this.crashTimestamps.push(now);
    this.writeCrashDump(code, signal, interrupted);
    if (this.crashTimestamps.length > Agent.MAX_AUTO_RESTARTS) {
      const result = await dialog.showMessageBox({ type: "error", title: "Crab Desktop — Agent crashed repeatedly", message: `Desktop services stopped ${this.crashTimestamps.length} times in the last 30 seconds.`, buttons: ["Restart", "Quit"], defaultId: 0, cancelId: 1 });
      if (this.stopping) return;
      if (result.response !== 0) { app.quit(); return; }
      this.crashTimestamps = [];
    }
    this.emit("notification", "agent/crash_restart", { message: "Desktop services stopped — restarting.", code, signal });
    try {
      await this.start();
      this.emit("notification", "agent/restart_complete", { message: "Desktop services restarted. Repository recovery results are reported separately." });
    } catch (error) {
      this.emit("notification", "agent/failed", { message: error instanceof Error ? error.message : String(error) });
    }
  }

  private writeCrashDump(code: number | null, signal: string | null, interrupted: string[]): void {
    try {
      const directory = path.join(app.getPath("userData"), "crash");
      fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
      const timestamp = new Date().toISOString();
      fs.writeFileSync(path.join(directory, `${timestamp.replace(/[:.]/g, "-")}.json`), JSON.stringify({ timestamp, exit_code: code, signal, last_500_log_lines: this.recentStderrLines.slice(-500), pending_requests: interrupted }, null, 2), { encoding: "utf8", mode: 0o600 });
    } catch { process.stderr.write("[crab-desktop] failed to write crash dump\n"); }
  }
}
