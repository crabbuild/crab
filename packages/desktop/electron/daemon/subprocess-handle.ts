/**
 * node-pty wrapper implementing the SubprocessHandle interface.
 *
 * This abstraction enables testability (inject fake PTY in tests) and
 * encapsulates fd-management hazards:
 * - Explicit dispose() releases the master fd (node-pty leaks it until GC)
 * - After dispose(), kill() is neutralized to prevent SIGHUP-to-recycled-PID
 *
 * On POSIX, forceKill bypasses neutralization with SIGKILL and must precede
 * disposal. Windows termination belongs to ConPTY, which enumerates console
 * members before killing them; terminating the shell first can orphan children.
 */

import type { IPty } from "node-pty";
import type { SubprocessHandle } from "./types";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { getBashRcfileContent, getZshRcfileContent } from "./shell-ready";

export interface SubprocessSpawnOptions {
  file: string;
  args?: string[];
  cols: number;
  rows: number;
  cwd?: string;
  env?: Record<string, string>;
  shellReadySupported?: boolean;
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
 * signals to a recycled PID. POSIX forceKill is not neutralized by dispose;
 * callers must call it first. Windows requests console termination only once.
 */
export class NodePtySubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private pty: IPty;
  private disposed = false;
  private conptyClosing = false;
  private readonly closed: Promise<void>;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];

  constructor(pty: IPty, private readonly cleanup = () => {}) {
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

    this.closed = new Promise(resolve => {
      this.pty.onExit(({ exitCode }) => {
        // Natural exit already closed the console. Enumerating it again can
        // fail AttachConsole and leave node-pty's helper waiting five seconds.
        this.conptyClosing = true;
        this.cleanup();
        resolve();
        for (const cb of this.exitCallbacks) cb(exitCode);
      });
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
    if (process.platform === "win32") this.closeConpty();
    else this.pty.kill(signal);
  }

  forceKill(): void {
    if (process.platform === "win32") {
      this.closeConpty();
      return;
    }
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
   * - On Windows, the returned promise waits for the output socket to close
   */
  dispose(): void | Promise<void> {
    if (!this.disposed) {
      this.disposed = true;
      this.dataCallbacks = [];
      this.exitCallbacks = [];
      if (process.platform === "win32") this.closeConpty();
      else this.pty.kill();
      this.cleanup();
    }
    // ConPTY termination runs asynchronously. Its exit event follows process
    // termination and output-pipe closure; callers must join it before exit.
    if (process.platform === "win32") return this.closed;
  }

  private closeConpty(): void {
    // A second kill can enumerate after the console has closed. Let the first
    // request retain ownership of node-pty's asynchronous process-tree teardown.
    if (this.conptyClosing) return;
    this.conptyClosing = true;
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
    const env = { ...process.env, ...opts.env } as Record<string, string>;
    const shell = path.basename(opts.file);
    let args = opts.args ?? [];
    let wrapper: string | undefined;
    if (opts.shellReadySupported && (shell === "bash" || shell === "zsh")) {
      wrapper = fs.mkdtempSync(path.join(os.tmpdir(), "crab-shell-"));
      if (shell === "bash") {
        fs.writeFileSync(path.join(wrapper, "bashrc"), getBashRcfileContent());
        args = ["--rcfile", path.join(wrapper, "bashrc"), "-i"];
      } else {
        env.CRAB_ORIG_ZDOTDIR = env.ZDOTDIR || env.HOME;
        // Load login configuration from its original location. The wrapper
        // owns readiness only; user files and subsequent shells keep their home.
        fs.writeFileSync(path.join(wrapper, ".zshenv"), '[[ -f "$CRAB_ORIG_ZDOTDIR/.zshenv" ]] && source "$CRAB_ORIG_ZDOTDIR/.zshenv"\n');
        fs.writeFileSync(path.join(wrapper, ".zprofile"), '[[ -f "$CRAB_ORIG_ZDOTDIR/.zprofile" ]] && source "$CRAB_ORIG_ZDOTDIR/.zprofile"\n');
        fs.writeFileSync(path.join(wrapper, ".zshrc"), getZshRcfileContent() + '\nZDOTDIR="$CRAB_ORIG_ZDOTDIR"\n');
        env.ZDOTDIR = wrapper;
        args = ["--login", "-i"];
      }
    }
    const cleanup = () => { if (wrapper) fs.rmSync(wrapper, { recursive: true, force: true }); };
    try {
      const pty = nodePty.spawn(opts.file, args, {
      cols: opts.cols,
      rows: opts.rows,
      cwd: opts.cwd,
      env,
      });
      return new NodePtySubprocessHandle(pty, cleanup);
    } catch (error) { cleanup(); throw error; }
  };
}
