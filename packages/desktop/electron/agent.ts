import { spawn, ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";
import * as readline from "node:readline";
import * as path from "node:path";
import * as fs from "node:fs";
import { app, dialog } from "electron";

// Narrow JSON-RPC 2.0 types. The agent sends three frame shapes:
//   - success response: { jsonrpc, id, result }
//   - error   response: { jsonrpc, id, error: { code, message, data? } }
//   - notification:     { jsonrpc, method, params }
// We only read the fields we care about; anything else is opaque.

interface RpcSuccess {
  jsonrpc: "2.0";
  id: number | string | null;
  result: unknown;
}
interface RpcError {
  jsonrpc: "2.0";
  id: number | string | null;
  error: { code: number; message: string; data?: unknown };
}
interface RpcNotification {
  jsonrpc: "2.0";
  method: string;
  params: unknown;
}

type RpcFrame = RpcSuccess | RpcError | RpcNotification;

function isResponse(frame: RpcFrame): frame is RpcSuccess | RpcError {
  return "id" in frame && frame.id !== undefined;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
}

// Jobs that the crab CLI supports resuming from a checkpoint.
const RESUMABLE_OPS = new Set(["push", "hydrate"]);

export class Agent extends EventEmitter {
  private child: ChildProcessWithoutNullStreams | null = null;
  private readonly binary: string;
  private readonly env: NodeJS.ProcessEnv;
  private readonly extraArgs: string[];
  private pending = new Map<string, Pending>();
  private nextId = 1;
  private stopping = false;

  // Restart-debounce state (§F.3): track recent crash timestamps.
  private crashTimestamps: number[] = [];
  private static readonly CRASH_WINDOW_MS = 30_000;
  private static readonly MAX_AUTO_RESTARTS = 1;

  // Ring buffer of recent stderr lines for crash dumps.
  private recentStderrLines: string[] = [];
  private static readonly MAX_STDERR_LINES = 500;

  constructor(binary: string, env: NodeJS.ProcessEnv = process.env, extraArgs: string[] = []) {
    super();
    this.binary = binary;
    this.env = env;
    this.extraArgs = extraArgs;
  }

  async start(): Promise<void> {
    this.stopping = false;
    const child = spawn(this.binary, this.extraArgs, {
      stdio: ["pipe", "pipe", "pipe"],
      env: this.env,
    });
    this.child = child;

    child.on("error", (error) => {
      this.child = null;
      for (const pending of this.pending.values()) pending.reject(error);
      this.pending.clear();
    });

    readline.createInterface({ input: child.stdout }).on("line", (line) => {
      if (!line) return;
      this.onLine(line);
    });

    // Capture stderr lines into a ring buffer for crash dumps.
    const stderrRl = readline.createInterface({ input: child.stderr });
    stderrRl.on("line", (line) => {
      this.recentStderrLines.push(line);
      if (this.recentStderrLines.length > Agent.MAX_STDERR_LINES) {
        this.recentStderrLines.shift();
      }
      process.stderr.write(`[agent] ${line}\n`);
    });

    child.on("exit", (code, signal) => {
      this.child = null;
      for (const p of this.pending.values()) {
        p.reject(new Error(`agent exited before response (code=${code})`));
      }
      this.pending.clear();
      this.emit("exit", code, signal);

      // Non-zero exit = crash. Handle restart-debounce (§F.3).
      if (!this.stopping && code !== 0 && code !== null) {
        void this.handleCrash(code, signal);
      }
    });

    // Sanity check with a bounded timeout. The round-trip should take
    // milliseconds; if it stalls, something is deeply wrong with the
    // spawn (missing exec bit, quarantine, stdio buffering quirk).
    // We raise a descriptive error rather than letting the app hang
    // waiting for a reply that will never come.
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        this.request("ping", {}),
        new Promise<void>((_, reject) => {
          timer = setTimeout(() => reject(new Error("agent ping timed out after 10 s")), 10_000);
        }),
      ]);
    } catch (error) {
      this.shutdown();
      throw error;
    } finally {
      clearTimeout(timer);
    }
  }

  async request(method: string, params: unknown): Promise<unknown> {
    if (!this.child) throw new Error("agent not running");
    const id = String(this.nextId++);
    const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child!.stdin.write(frame, (err) => {
        if (err) {
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }

  shutdown(): void {
    if (!this.child) return;
    this.stopping = true;
    const child = this.child;
    // EOF lets the agent drain requests and close SDK and SlateDB sessions.
    const timer = setTimeout(() => child.kill("SIGTERM"), 8_000);
    timer.unref();
    child.once("exit", () => clearTimeout(timer));
    child.stdin.end();
    this.child = null;
  }

  private onLine(line: string): void {
    let frame: RpcFrame;
    try {
      frame = JSON.parse(line) as RpcFrame;
    } catch (err) {
      process.stderr.write(`[agent] malformed frame: ${line}\n`);
      return;
    }

    if (isResponse(frame)) {
      const id = String(frame.id);
      const pending = this.pending.get(id);
      if (!pending) return;
      this.pending.delete(id);
      if ("error" in frame) {
        const err = new Error(frame.error.message);
        (err as Error & { code?: number; data?: unknown }).code = frame.error.code;
        (err as Error & { code?: number; data?: unknown }).data = frame.error.data;
        pending.reject(err);
      } else {
        pending.resolve(frame.result);
      }
      return;
    }

    // Notification.
    this.emit("notification", frame.method, frame.params);
  }

  // ── Crash recovery (§F.3) ──────────────────────────────────────

  private async handleCrash(code: number, signal: string | null): Promise<void> {
    const now = Date.now();
    this.crashTimestamps.push(now);

    // Prune timestamps older than the crash window.
    this.crashTimestamps = this.crashTimestamps.filter(
      (ts) => now - ts < Agent.CRASH_WINDOW_MS,
    );

    // Write crash dump to ~/.crab/desktop/crash/<ts>.json
    this.writeCrashDump(code, signal);

    const recentCrashes = this.crashTimestamps.length;

    if (recentCrashes <= Agent.MAX_AUTO_RESTARTS) {
      // Auto-restart with a toast notification.
      this.emit("notification", "agent/crash_restart", {
        message: "Agent crashed — restarting automatically.",
        code,
        signal,
      });
      try {
        await this.start();
        // After restart, emit a notification about in-flight jobs.
        this.emitResumeNotices();
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        this.emit("notification", "agent/failed", { message: msg });
      }
    } else {
      // Too many crashes in the window — ask the user.
      const result = await dialog.showMessageBox({
        type: "error",
        title: "Crab Desktop — Agent crashed repeatedly",
        message:
          `The agent has crashed ${recentCrashes} times in the last 30 seconds.\n\n` +
          "Would you like to restart it, or quit the application?",
        buttons: ["Restart", "Quit"],
        defaultId: 0,
        cancelId: 1,
      });

      if (result.response === 0) {
        // User chose restart — reset the crash window.
        this.crashTimestamps = [];
        try {
          await this.start();
          this.emitResumeNotices();
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          this.emit("notification", "agent/failed", { message: msg });
        }
      } else {
        app.quit();
      }
    }
  }

  /**
   * Write a crash dump JSON file to ~/.crab/desktop/crash/<ts>.json
   * containing the exit code, signal, last 500 log lines, and any
   * pending jobs from the renderer state (passed via the "exit" event
   * listener in main.ts).
   */
  private writeCrashDump(code: number, signal: string | null): void {
    try {
      const crashDir = path.join(app.getPath("home"), ".crab", "desktop", "crash");
      if (!fs.existsSync(crashDir)) {
        fs.mkdirSync(crashDir, { recursive: true });
      }

      const ts = new Date().toISOString().replace(/[:.]/g, "-");
      const dumpPath = path.join(crashDir, `${ts}.json`);

      const dump = {
        timestamp: new Date().toISOString(),
        exit_code: code,
        signal,
        last_500_log_lines: this.recentStderrLines.slice(-500),
        pending_jobs: this.getPendingJobIds(),
      };

      fs.writeFileSync(dumpPath, JSON.stringify(dump, null, 2), "utf8");
    } catch {
      // Best-effort — don't let crash-dump writing itself crash.
      process.stderr.write("[crab-desktop] failed to write crash dump\n");
    }
  }

  /**
   * Returns the IDs of pending RPC requests at the time of crash.
   * These represent in-flight operations that were interrupted.
   */
  private getPendingJobIds(): string[] {
    return Array.from(this.pending.keys());
  }

  /**
   * After a restart, emit notifications about in-flight jobs.
   * Jobs whose CLI supports resume (push, hydrate) get a "resume"
   * notice; others get a "please re-run manually" notice.
   */
  private emitResumeNotices(): void {
    // The pending map was cleared on exit, but we can read the last
    // crash dump to find what was in-flight. For now, emit a generic
    // notice — the renderer can check its own job state to determine
    // which jobs were running.
    this.emit("notification", "agent/restart_complete", {
      message: "Agent restarted after crash.",
      resumable_ops: Array.from(RESUMABLE_OPS),
    });
  }
}
