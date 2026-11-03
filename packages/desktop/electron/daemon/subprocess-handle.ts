/**
 * node-pty wrapper implementing the SubprocessHandle interface.
 *
 * This abstraction enables testability (inject fake PTY in tests) and
 * encapsulates fd-management hazards:
 * - Explicit dispose() releases the master fd (node-pty leaks it until GC)
 * - After dispose(), kill() is neutralized to prevent SIGHUP-to-recycled-PID
 *
 * The `forceKill()` method uses `process.kill(pid, 'SIGKILL')` directly,
 * bypassing the neutralization — callers must ensure forceKill is called
 * BEFORE dispose() when both are needed (kill timeout path).
 */

import type { IPty } from "node-pty";
import type { SubprocessHandle } from "./types";

export interface SubprocessSpawnOptions {
  file: string;
  args?: string[];
  cols: number;
  rows: number;
  cwd?: string;
  env?: Record<string, string>;
}

/**
 * Factory type for spawning subprocesses. The daemon injects this so
 * tests can provide a mock implementation.
 */
export type SubprocessSpawner = (opts: SubprocessSpawnOptions) => SubprocessHandle;

/**
 * Production SubprocessHandle wrapping a node-pty IPty instance.
 *
 * After dispose(), the kill() method becomes a no-op to prevent sending
 * signals to a recycled PID. forceKill() is NOT neutralized by dispose —
 * callers must call forceKill BEFORE dispose when both are needed.
 */
export class NodePtySubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private pty: IPty;
  private disposed = false;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];

  constructor(pty: IPty) {
    this.pty = pty;
    this.pid = pty.pid;
    // node-pty exposes the master fd via the internal _fd property
    this.fd = (pty as unknown as { _fd: number })._fd ?? -1;

    this.pty.onData((data) => {
      if (this.disposed) return;
      for (const cb of this.dataCallbacks) {
        cb(data);
      }
    });

    this.pty.onExit(({ exitCode }) => {
      for (const cb of this.exitCallbacks) {
        cb(exitCode);
      }
    });
  }

  write(data: string): void {
    if (this.disposed) return;
    this.pty.write(data);
  }

  resize(cols: number, rows: number): void {
    if (this.disposed) return;
    this.pty.resize(cols, rows);
  }

  kill(signal?: string): void {
    // Neutralized after dispose to prevent SIGHUP-to-recycled-PID
    if (this.disposed) return;
    this.pty.kill(signal);
  }

  forceKill(): void {
    // Uses process.kill directly — NOT neutralized by dispose.
    // Callers MUST call forceKill BEFORE dispose when both are needed.
    try {
      process.kill(this.pid, "SIGKILL");
    } catch {
      // Process already exited — ignore ESRCH
    }
  }

  signal(sig: string): void {
    if (this.disposed) return;
    this.pty.kill(sig);
  }

  pause(): void {
    if (this.disposed) return;
    this.pty.pause();
  }

  resume(): void {
    if (this.disposed) return;
    this.pty.resume();
  }

  onData(cb: (data: string) => void): void {
    this.dataCallbacks.push(cb);
  }

  onExit(cb: (exitCode: number) => void): void {
    this.exitCallbacks.push(cb);
  }

  /**
   * Release the PTY master fd. After this call:
   * - kill() becomes a no-op (SIGHUP neutralization)
   * - write/resize/signal/pause/resume become no-ops
   * - The underlying node-pty socket is destroyed
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.dataCallbacks = [];
    this.exitCallbacks = [];
    this.pty.kill();
  }
}

/**
 * Production spawner using node-pty. Used by the daemon server.
 */
export function createNodePtySpawner(): SubprocessSpawner {
  // Lazy require to avoid loading node-pty in test environments
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const nodePty = require("node-pty") as typeof import("node-pty");

  return (opts: SubprocessSpawnOptions): SubprocessHandle => {
    const pty = nodePty.spawn(opts.file, opts.args ?? [], {
      cols: opts.cols,
      rows: opts.rows,
      cwd: opts.cwd,
      env: opts.env as Record<string, string>,
    });
    return new NodePtySubprocessHandle(pty);
  };
}
