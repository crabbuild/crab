/**
 * Session — individual PTY session with state machine and headless emulator.
 *
 * State machine: created → running → exiting → exited
 *
 * Three distinct teardown paths (order is load-bearing):
 * 1. Natural exit (handleSubprocessExit): subprocess.dispose() directly
 * 2. Kill timeout (forceDispose): forceKill() BEFORE teardownSubprocess()
 * 3. Graceful shutdown (forceKillAndDisposeSubprocess): same as kill timeout,
 *    bypasses the 5s timer
 *
 * The SubprocessHandle abstraction enables testability — inject fake PTY
 * in tests via the spawner function.
 */

import type {
  AgentMeta,
  SessionState,
  ShellReadyState,
  StreamClient,
  SubprocessHandle,
  TerminalSnapshot,
} from "./types";
import { HeadlessEmulator } from "./headless-emulator";
import { PostReadyFlushGate, ShellReadyScanner } from "./shell-ready";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Time to wait for SIGTERM before sending SIGKILL. */
const KILL_TIMEOUT_MS = 5000;

/** Shell-ready timeout: if no marker after this, mark as timed_out. */
const SHELL_READY_TIMEOUT_MS = 15_000;

// ---------------------------------------------------------------------------
// Session Options
// ---------------------------------------------------------------------------

export interface SessionOptions {
  sessionId: string;
  cols: number;
  rows: number;
  cwd?: string | null;
  subprocess: SubprocessHandle;
  shellReadySupported: boolean;
  agentMeta?: AgentMeta;
  /** Called when the session exits (natural or forced). */
  onExit?: (sessionId: string, exitCode: number) => void;
}

// ---------------------------------------------------------------------------
// Session Class
// ---------------------------------------------------------------------------

export class Session {
  readonly sessionId: string;
  readonly pid: number;
  readonly createdAt: number;
  readonly agentMeta?: AgentMeta;
  /** The cwd used when the PTY was spawned/adopted. */
  readonly initialCwd: string | null;

  private _state: SessionState = "created";
  private _shellState: ShellReadyState;
  private _exitCode: number | null = null;
  private _cols: number;
  private _rows: number;
  private _lastDataAt: number = 0;
  /** When true, the headless emulator does not receive subprocess output. */
  private _highThroughput: boolean = false;

  private subprocess: SubprocessHandle;
  private emulator: HeadlessEmulator;
  private scanner: ShellReadyScanner;
  private flushGate: PostReadyFlushGate;

  private clients: Map<symbol, StreamClient> = new Map();
  private stdinQueue: string[] = [];
  private killTimer: ReturnType<typeof setTimeout> | null = null;
  private shellReadyTimer: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;
  private onExitCallback?: (sessionId: string, exitCode: number) => void;

  constructor(opts: SessionOptions) {
    this.sessionId = opts.sessionId;
    this.pid = opts.subprocess.pid;
    this.createdAt = Date.now();
    this.agentMeta = opts.agentMeta;
    this.initialCwd = opts.cwd ?? null;
    this._cols = opts.cols;
    this._rows = opts.rows;
    this.subprocess = opts.subprocess;
    this.onExitCallback = opts.onExit;

    this._shellState = opts.shellReadySupported ? "pending" : "unsupported";

    // Headless emulator for state tracking
    this.emulator = new HeadlessEmulator({ cols: opts.cols, rows: opts.rows });

    // Shell-ready scanner and flush gate
    this.scanner = new ShellReadyScanner();
    this.flushGate = new PostReadyFlushGate(() => this.flushStdinQueue());

    // Wire subprocess events
    this.subprocess.onData((data) => this.handleSubprocessData(data));
    this.subprocess.onExit((code) => this.handleSubprocessExit(code));

    // Start shell-ready timeout if supported
    if (opts.shellReadySupported) {
      this.shellReadyTimer = setTimeout(() => {
        this.shellReadyTimer = null;
        if (this._shellState === "pending") {
          this._shellState = "timed_out";
          this.flushStdinQueue();
        }
      }, SHELL_READY_TIMEOUT_MS);
    }

    // Transition to running
    this._state = "running";
  }

  // -------------------------------------------------------------------------
  // Public accessors
  // -------------------------------------------------------------------------

  get state(): SessionState {
    return this._state;
  }

  get shellState(): ShellReadyState {
    return this._shellState;
  }

  get exitCode(): number | null {
    return this._exitCode;
  }

  get cols(): number {
    return this._cols;
  }

  get rows(): number {
    return this._rows;
  }

  get isAlive(): boolean {
    return this._state === "running" || this._state === "created";
  }

  /**
   * Timestamp of the last data chunk received from the subprocess.
   * Zero until the first data chunk arrives.
   */
  get lastDataAt(): number {
    return this._lastDataAt;
  }

  /**
   * Get the PTY master file descriptor (for fd-handoff during upgrades).
   */
  get fd(): number {
    return this.subprocess.fd;
  }

  // -------------------------------------------------------------------------
  // Session operations
  // -------------------------------------------------------------------------

  /**
   * Write data to the PTY stdin. If shell-ready barrier is pending,
   * the data is queued until the barrier resolves.
   */
  write(data: string): void {
    if (this._state !== "running") return;

    if (this._shellState === "pending") {
      this.stdinQueue.push(data);
      return;
    }

    this.subprocess.write(data);
  }

  /**
   * Resize the PTY and headless emulator.
   */
  resize(cols: number, rows: number): void {
    if (this._state !== "running") return;

    // Clamp to minimum 1×1
    const clampedCols = Math.max(1, cols);
    const clampedRows = Math.max(1, rows);

    this._cols = clampedCols;
    this._rows = clampedRows;
    this.subprocess.resize(clampedCols, clampedRows);
    this.emulator.resize(clampedCols, clampedRows);
  }

  /**
   * Kill the session: SIGTERM + 5s SIGKILL timer.
   * Transitions state to "exiting".
   */
  kill(): void {
    if (this._state !== "running") return;

    this._state = "exiting";
    this.subprocess.kill("SIGTERM");

    this.killTimer = setTimeout(() => {
      this.killTimer = null;
      this.forceDispose();
    }, KILL_TIMEOUT_MS);
  }

  /**
   * Send an arbitrary signal to the subprocess.
   */
  signal(sig: string): void {
    if (this._state !== "running") return;
    this.subprocess.signal(sig);
  }

  /**
   * Get the current terminal snapshot from the headless emulator.
   */
  getSnapshot(): TerminalSnapshot | null {
    if (this._state === "exited" && this.disposed) return null;
    return this.emulator.getSnapshot();
  }

  /**
   * Get the current working directory from OSC-7 tracking.
   */
  getCwd(): string | null {
    return this.emulator.getCwd();
  }

  /**
   * Clear the headless emulator's scrollback buffer.
   */
  clearScrollback(): void {
    this.emulator.clearScrollback();
  }

  /**
   * Pause the subprocess PTY read stream (backpressure).
   */
  pauseSubprocess(): void {
    if (this._state !== "running") return;
    this.subprocess.pause();
  }

  /**
   * Resume the subprocess PTY read stream (backpressure relief).
   */
  resumeSubprocess(): void {
    if (this._state !== "running") return;
    this.subprocess.resume();
  }

  /**
   * Toggle high-throughput mode. When on, the headless emulator does not
   * receive subprocess output, reducing CPU cost for fast-scrolling sessions.
   * Snapshots are considered stale while high-throughput mode is active.
   */
  setHighThroughput(on: boolean): void {
    this._highThroughput = on;
  }

  // -------------------------------------------------------------------------
  // Client management (symbol-based tokens)
  // -------------------------------------------------------------------------

  /**
   * Attach a stream client. Returns a symbol token for detaching.
   */
  attachClient(client: StreamClient): symbol {
    const token = Symbol(`client:${client.id}`);
    this.clients.set(token, client);
    return token;
  }

  /**
   * Detach a specific client by its token.
   */
  detachClient(token: symbol): void {
    this.clients.delete(token);
  }

  /**
   * Detach all clients (used on reattach or dispose).
   */
  detachAllClients(): void {
    this.clients.clear();
  }

  /**
   * Number of currently attached clients.
   */
  get clientCount(): number {
    return this.clients.size;
  }

  // -------------------------------------------------------------------------
  // Teardown paths
  // -------------------------------------------------------------------------

  /**
   * Path 3: Graceful shutdown — forceKill + dispose without the 5s timer.
   * Called by TerminalHost.dispose() to reap stubborn children immediately.
   */
  forceKillAndDisposeSubprocess(): void {
    if (this.disposed) return;

    this.clearTimers();

    // forceKill BEFORE dispose — order is load-bearing
    this.subprocess.forceKill();
    this.teardownSubprocess();
    this.finalizeExit(-1);
  }

  /**
   * Full dispose of the session (cleanup emulator, clients, timers).
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    this.clearTimers();
    this.flushGate.clear();
    this.scanner.reset();
    this.detachAllClients();
    this.emulator.dispose();
    this.stdinQueue = [];
  }

  // -------------------------------------------------------------------------
  // Private: subprocess event handlers
  // -------------------------------------------------------------------------

  private handleSubprocessData(data: string): void {
    if (this.disposed) return;

    // Track last data timestamp
    this._lastDataAt = Date.now();

    // Shell-ready marker scanning
    let output: string;
    if (this._shellState === "pending") {
      const result = this.scanner.scan(data);
      output = result.output;

      if (result.matched) {
        this._shellState = "ready";
        this.clearShellReadyTimer();
        // Arm the flush gate — it will flush stdin after prompt draw + delay
        this.flushGate.arm();
      }
    } else {
      output = data;
    }

    // Feed to headless emulator for state tracking (skipped in high-throughput mode)
    if (!this._highThroughput) {
      this.emulator.write(output);
    }

    // Notify flush gate of data (for post-ready delay timing)
    if (this.flushGate.isPending) {
      this.flushGate.notifyData();
    }

    // Broadcast to attached clients
    const buf = Buffer.from(output, "utf-8");
    for (const client of this.clients.values()) {
      client.sendData(this.sessionId, buf);
    }
  }

  private handleSubprocessExit(exitCode: number): void {
    if (this.disposed) return;

    // Path 1: Natural exit — dispose subprocess directly
    this.subprocess.dispose();
    this.clearTimers();
    this.finalizeExit(exitCode);
  }

  // -------------------------------------------------------------------------
  // Private: teardown helpers
  // -------------------------------------------------------------------------

  /**
   * Path 2: Kill timeout — forceKill BEFORE teardownSubprocess.
   * Order is load-bearing: teardownSubprocess's dispose() neutralizes
   * kill() on POSIX, but forceKill uses process.kill directly.
   */
  private forceDispose(): void {
    if (this.disposed) return;

    // forceKill BEFORE dispose — order is load-bearing
    this.subprocess.forceKill();
    this.teardownSubprocess();
    this.finalizeExit(-1);
  }

  /**
   * Shared teardown: dispose the subprocess handle (releases master fd,
   * neutralizes future kill() calls).
   */
  private teardownSubprocess(): void {
    this.subprocess.dispose();
  }

  /**
   * Finalize exit: set state, notify clients, invoke callback.
   */
  private finalizeExit(exitCode: number): void {
    this._exitCode = exitCode;
    this._state = "exited";

    // Notify attached clients of exit
    for (const client of this.clients.values()) {
      client.sendExit(this.sessionId, exitCode);
    }

    // Invoke the onExit callback
    if (this.onExitCallback) {
      this.onExitCallback(this.sessionId, exitCode);
    }

    // Dispose the session resources
    this.dispose();
  }

  // -------------------------------------------------------------------------
  // Private: stdin queue and timers
  // -------------------------------------------------------------------------

  private flushStdinQueue(): void {
    if (this._state !== "running") return;

    for (const data of this.stdinQueue) {
      this.subprocess.write(data);
    }
    this.stdinQueue = [];
  }

  private clearTimers(): void {
    if (this.killTimer !== null) {
      clearTimeout(this.killTimer);
      this.killTimer = null;
    }
    this.clearShellReadyTimer();
  }

  private clearShellReadyTimer(): void {
    if (this.shellReadyTimer !== null) {
      clearTimeout(this.shellReadyTimer);
      this.shellReadyTimer = null;
    }
  }
}
