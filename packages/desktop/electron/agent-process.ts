import { spawn, type ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import { PassThrough, Writable, type Readable } from "node:stream";
import { getExpandedEnv } from "./env/userShellEnv";

export interface AgentProcess extends EventEmitter {
  stdin: Writable | null;
  stdout: Readable | null;
  stderr: Readable | null;
  kill(signal?: NodeJS.Signals | number): boolean;
}

export interface OwnedAgentProcess {
  process: AgentProcess;
  closed: Promise<void>;
}

export type AgentProcessFactory = (binary: string, args: string[], options: {
  cwd: string;
  env?: Record<string, string>;
}) => OwnedAgentProcess;

class LocalAgentProcess extends EventEmitter implements AgentProcess {
  readonly stdin: Writable;
  readonly stdout = new PassThrough();
  readonly stderr = new PassThrough();
  readonly closed: Promise<void>;
  private child?: ChildProcess;
  private group?: number;
  private groupGone = false;
  private finished = false;
  private failure?: Error;
  private stdioExit?: [number | null, NodeJS.Signals | null];
  private resolveClosed!: () => void;
  private rejectClosed!: (error: Error) => void;
  private resolveStarted!: (child: ChildProcess) => void;
  private rejectStarted!: (error: Error) => void;

  constructor(binary: string, args: string[], options: { cwd: string; env?: Record<string, string> }) {
    super();
    const started = new Promise<ChildProcess>((resolve, reject) => { this.resolveStarted = resolve; this.rejectStarted = reject; });
    void started.catch(() => {});
    // Keep write acknowledgement at the real pipe, including while discovery
    // is pending. Buffered initialization must not claim delivery before spawn.
    this.stdin = new Writable({
      write: (bytes, encoding, done) => { void started.then(child => {
        if (!child.stdin) { done(new Error("Agent stdin unavailable")); return; }
        child.stdin.write(bytes, encoding, done);
      }, done); },
      final: done => { void started.then(child => {
        if (!child.stdin) { done(); return; }
        child.stdin.end(done);
      }, done); },
    });
    this.closed = new Promise<void>((resolve, reject) => { this.resolveClosed = resolve; this.rejectClosed = reject; });
    void this.closed.catch(() => {});
    void this.start(binary, args, options).catch(reason => {
      if (this.finished) return;
      if (this.child) { this.kill("SIGKILL"); this.fail(reason); return; }
      const error = reason instanceof Error ? reason : new Error("Agent environment preparation failed", { cause: reason });
      this.stdin.destroy();
      this.rejectStarted(error);
      this.emit("error", error);
      this.complete(1, null, true);
    });
  }

  private async start(binary: string, args: string[], options: { cwd: string; env?: Record<string, string> }): Promise<void> {
    const env = await getExpandedEnv(options.env);
    if (this.finished) return;
    const child = spawn(binary, args, { cwd: options.cwd, env, detached: process.platform !== "win32", windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
    this.child = child;
    this.group = process.platform === "win32" ? undefined : child.pid;
    child.stdout?.pipe(this.stdout);
    child.stderr?.pipe(this.stderr);
    // The owned writable forwards write failures through its callback. The
    // raw pipe must not emit an unhandled second error during process exit.
    child.stdin?.on("error", () => {});
    child.on("error", error => this.emit("error", error));
    child.once("close", (code, signal) => {
      this.stdioExit = [code, signal];
      // Failed spawn has no group. Windows retains its existing leader-only
      // boundary; descendant closure there still needs job-object ownership.
      if (this.group === undefined) this.complete(code, signal);
    });
    child.once("exit", (code, signal) => {
      this.emit("exit", code, signal);
      if (this.group === undefined) return;
      // A dead provider cannot service the conversation. Retire its group even
      // when inherited stdio prevents Node from emitting child.close.
      void this.retire().then(() => {
        if (!this.failure && this.stdioExit) this.complete(...this.stdioExit);
      }).catch(error => this.fail(error));
    });
    this.resolveStarted(child);
  }

  private complete(code: number | null, signal: NodeJS.Signals | null, emitExit = false): void {
    if (this.finished) return;
    this.finished = true;
    this.groupGone = true;
    this.stdin.destroy();
    for (const stream of [this.stdout, this.stderr]) if (!stream.writableEnded) stream.end();
    if (emitExit) this.emit("exit", code, signal);
    this.emit("close", code, signal);
    this.resolveClosed();
  }

  kill(signal: NodeJS.Signals | number = "SIGTERM"): boolean {
    if (this.finished || this.groupGone) return false;
    if (!this.child) {
      if (signal === 0) return false;
      // Discovery is shared; revoke only this launch and never spawn from its
      // late reply. No physical process exists to wait for in this phase.
      this.stdin.destroy();
      this.rejectStarted(new Error("Agent process launch was cancelled"));
      this.complete(null, typeof signal === "string" ? signal : null, true);
      return true;
    }
    if (this.group === undefined) return this.child.kill(signal);
    try { return this.signal(signal); } catch (error: unknown) { this.fail(error); return false; }
  }

  private signal(signal: NodeJS.Signals | number): boolean {
    if (this.groupGone || this.group === undefined) return false;
    try { process.kill(-this.group, signal); return true; } catch (error: unknown) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
      this.groupGone = true;
      return false;
    }
  }

  private async retire(): Promise<void> {
    const started = Date.now();
    let sent: NodeJS.Signals | undefined;
    let denied: unknown;
    while (!this.failure) {
      try {
        if (this.signal(0)) {
          const signal = Date.now() - started < 5000 ? "SIGTERM" : "SIGKILL";
          if (signal !== sent) { this.signal(signal); sent = signal; }
        }
        if (this.groupGone && this.stdioExit) return;
      } catch (error: unknown) {
        // Darwin can deny a probe/signal for an exiting, zombie-only group.
        // Wait for ESRCH; permission denial never proves writer closure.
        if ((error as NodeJS.ErrnoException).code !== "EPERM") throw error;
        denied = error;
      }
      if (Date.now() - started >= 10_000) throw new Error("Native agent process group closure is unconfirmed", { cause: denied });
      await new Promise(resolve => setTimeout(resolve, 25));
    }
  }

  private fail(reason: unknown): void {
    if (this.failure) return;
    this.failure = new Error("Native agent process group closure is unconfirmed", { cause: reason });
    this.rejectClosed(this.failure);
    // No close event: adapters must retain their writer until closure is proven.
    this.emit("error", this.failure);
  }
}

/** Expands local credentials/PATH only at the local process boundary. */
export const spawnLocalAgent: AgentProcessFactory = (binary, args, options) => {
  const owner = new LocalAgentProcess(binary, args, options);
  return { process: owner, closed: owner.closed };
};

export class AgentProcessClosureError extends Error {
  constructor(cause: unknown) { super("Command process closure is unconfirmed", { cause }); }
}

export class WorkspaceCommandCancelledError extends Error {
  constructor() { super("Workspace command was cancelled"); }
}

/** Captures an argument-only command, retaining its owner through failure or timeout. */
export async function runAgentCommand(factory: AgentProcessFactory, binary: string, args: string[], options: {
  cwd: string; env?: Record<string, string>; signal?: AbortSignal; timeoutMs?: number;
}): Promise<{ stdout: string }> {
  const { cwd, env, signal, timeoutMs } = options;
  signal?.throwIfAborted();
  const owner = factory(binary, args, { cwd, env });
  const output: Buffer[][] = [[], []];
  const sizes = [0, 0];
  let failure: Error | undefined;
  let escalation: ReturnType<typeof setTimeout> | undefined;
  let exitCode: number | null = null;
  const stop = (error: Error) => {
    if (failure) return;
    failure = error;
    owner.process.kill("SIGTERM");
    // A command leader may ignore TERM and never enter exit-driven cleanup.
    // Escalate once, retaining its owner until physical closure is confirmed.
    escalation = setTimeout(() => owner.process.kill("SIGKILL"), 5000);
  };
  owner.process.on("error", (error: Error) => stop(error));
  owner.process.once("close", (code: number | null) => { exitCode = code; });
  for (const [index, stream] of [owner.process.stdout, owner.process.stderr].entries()) stream?.on("data", (bytes: Buffer) => {
    sizes[index] += bytes.length;
    if (sizes[index] <= (index === 0 ? 50 * 1024 * 1024 : 1024 * 1024)) output[index].push(bytes);
    else stop(new Error("Workspace command exceeded its output limit"));
  });
  // These commands send no stdin payload. An EOF racing process exit cannot
  // make delivery uncertain; physical closure remains the completion boundary.
  owner.process.stdin?.on("error", () => {});
  owner.process.stdin?.end();
  const abort = () => stop(signal?.reason instanceof Error ? signal.reason : new Error("Workspace command was cancelled", { cause: signal?.reason }));
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  const timer = timeoutMs === undefined ? undefined : setTimeout(() => stop(new Error("Workspace command timed out")), timeoutMs);
  try {
    try { await owner.closed; } catch (error: unknown) { throw new AgentProcessClosureError(error); }
    const stdout = Buffer.concat(output[0]).toString("utf8");
    const stderr = Buffer.concat(output[1]).toString("utf8");
    if (failure) throw failure;
    if (exitCode !== 0) throw Object.assign(new Error(`${binary} exited with code ${exitCode}: ${stderr}`), { code: exitCode, stderr });
    return { stdout };
  } finally { clearTimeout(timer); clearTimeout(escalation); signal?.removeEventListener("abort", abort); }
}
