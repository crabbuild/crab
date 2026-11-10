import { spawn, type ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import type { Readable, Writable } from "node:stream";
import { getExpandedEnv } from "./orchestrator-adapters";

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
  readonly stdin: Writable | null;
  readonly stdout: Readable | null;
  readonly stderr: Readable | null;
  readonly closed: Promise<void>;
  private groupGone = false;
  private failure?: Error;
  private stdioExit?: [number | null, NodeJS.Signals | null];
  private rejectClosed!: (error: Error) => void;

  constructor(child: ChildProcess, private readonly group: number) {
    super();
    this.stdin = child.stdin; this.stdout = child.stdout; this.stderr = child.stderr;
    this.closed = new Promise<void>((resolve, reject) => {
      this.rejectClosed = reject;
      child.once("close", (code, signal) => { this.stdioExit = [code, signal]; });
      child.once("exit", (code, signal) => {
        this.emit("exit", code, signal);
        // A dead provider cannot service the conversation. Retire its group
        // even when inherited stdio prevents Node from emitting child.close.
        void this.retire().then(() => {
          if (this.failure) return;
          this.emit("close", ...this.stdioExit!);
          resolve();
        }).catch(error => this.fail(error));
      });
    });
    void this.closed.catch(() => {});
    child.on("error", error => this.emit("error", error));
  }

  kill(signal: NodeJS.Signals | number = "SIGTERM"): boolean {
    if (this.groupGone) return false;
    try { return this.signal(signal); } catch (error: unknown) { this.fail(error); return false; }
  }

  private signal(signal: NodeJS.Signals | number): boolean {
    if (this.groupGone) return false;
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
  const env = getExpandedEnv(options.env);
  const child = spawn(binary, args, { cwd: options.cwd, env, detached: process.platform !== "win32", windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
  // Failed spawn has no PID/group. Windows still requires job-object ownership;
  // its existing leader-only boundary does not establish descendant closure.
  if (process.platform === "win32" || child.pid === undefined) {
    const closed = new Promise<void>(resolve => { child.once("close", () => resolve()); });
    return { process: child, closed };
  }
  const owner = new LocalAgentProcess(child, child.pid);
  return { process: owner, closed: owner.closed };
};
