/**
 * In-Process PTY Bridge.
 *
 * Encapsulates all node-pty terminal management that previously lived
 * inline in main.ts. Exposes handleSpawn / handleWrite / handleResize /
 * handleKill / handleSessionData / handleSessionExit / dispose so that
 * TermRouter can call these methods directly without knowing about IPC.
 *
 * IPC registration is intentionally absent — the router owns that.
 */

import * as path from "node:path";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Minimal BrowserWindow interface — only what the bridge needs. */
export interface BrowserWindowLike {
  isDestroyed(): boolean;
  webContents: {
    send(channel: string, ...args: unknown[]): void;
  };
}

/** Minimal App interface — only what the bridge needs. */
export interface AppLike {
  on(event: string, listener: (...args: unknown[]) => void): void;
}

/** IPty interface — mirrors the node-pty IPty shape without importing it. */
interface IPty {
  onData(listener: (data: string) => void): { dispose(): void };
  onExit(listener: (e: { exitCode: number; signal?: number }) => void): { dispose(): void };
  write(data: string): void;
  resize(cols: number, rows: number): void;
  kill(signal?: string): void;
}

export interface InProcessBridgeOptions {
  /** Returns the current main BrowserWindow (or null if destroyed). */
  getMainWindow: () => BrowserWindowLike | null;
  /**
   * Returns the default shell path for the current platform.
   * Injected so the bridge doesn't need to duplicate the detection logic.
   */
  getDefaultShell: () => string;
  /**
   * Builds the terminal environment, merging detected login-shell env with
   * any extra variables the caller provides.
   */
  buildTerminalEnv: (extraEnv?: Record<string, string>) => Record<string, string>;
  /**
   * Promise that resolves once login-shell environment detection completes.
   * The bridge awaits this (with a 2 s timeout) before spawning so the
   * terminal inherits the full user PATH.
   */
  shellEnvPromise: Promise<Record<string, string> | null>;
  /**
   * Per-session log writer. Owned by main.ts; the bridge calls it for every
   * chunk of PTY output so log files continue to work unchanged.
   */
  writeSessionLog: (sessionId: string, data: string) => void;
  /** The Electron app instance — used to hook before-quit. */
  app: AppLike;
}

export interface SpawnOptions {
  cwd: string;
  shell?: string;
  env?: Record<string, string>;
  startupCommand?: string;
}

export interface SpawnResult {
  id: string;
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

export class InProcessBridge {
  private readonly terminals = new Map<string, { term: IPty; exited: Promise<void>; closing: boolean }>();
  private termCounter = 0;
  private shuttingDown = false;

  private readonly getMainWindow: () => BrowserWindowLike | null;
  private readonly getDefaultShell: () => string;
  private readonly buildTerminalEnv: (extraEnv?: Record<string, string>) => Record<string, string>;
  private readonly shellEnvPromise: Promise<Record<string, string> | null>;
  private readonly writeSessionLog: (sessionId: string, data: string) => void;

  constructor(opts: InProcessBridgeOptions) {
    this.getMainWindow = opts.getMainWindow;
    this.getDefaultShell = opts.getDefaultShell;
    this.buildTerminalEnv = opts.buildTerminalEnv;
    this.shellEnvPromise = opts.shellEnvPromise;
    this.writeSessionLog = opts.writeSessionLog;

    opts.app.on("before-quit", () => {
      this.shuttingDown = true;
    });
  }

  // -------------------------------------------------------------------------
  // Shell helpers
  // -------------------------------------------------------------------------

  /**
   * Returns shell arguments for spawning a login/interactive shell.
   * - macOS/Linux: `["--login"]` so .zprofile/.bash_profile are sourced.
   * - Windows: empty — PowerShell/cmd don't use login flags.
   */
  private getLoginShellArgs(shellPath: string): string[] {
    if (process.platform === "win32") return [];
    const base = path.basename(shellPath);
    if (["zsh", "bash", "sh", "fish", "ksh"].includes(base)) {
      return ["--login"];
    }
    return [];
  }

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  /**
   * Spawn a new in-process PTY session.
   *
   * Rejects with `{ code: "app-shutting-down" }` once `app.before-quit`
   * has fired so callers get a typed error rather than a half-initialised
   * terminal.
   */
  async handleSpawn(opts: SpawnOptions): Promise<SpawnResult> {
    const id = `term-l-${++this.termCounter}`;
    const requestedShell = opts.shell ?? this.getDefaultShell();
    const requestedCwd = opts.cwd || process.env.HOME || "/";

    // Ensure shell env detection has completed before spawning.
    // Uses a short race so we don't block indefinitely if detection hangs.
    await Promise.race([
      this.shellEnvPromise,
      new Promise<void>((resolve) => setTimeout(resolve, 2000)),
    ]);

    if (this.shuttingDown) {
      throw Object.assign(new Error("Application is shutting down"), {
        code: "app-shutting-down",
      });
    }

    const termEnv = this.buildTerminalEnv({
      CRAB_TERMINAL: "1",
      CRAB_ROOT: requestedCwd,
      ...(opts.env ?? {}),
    });

    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const pty = require("node-pty") as {
      spawn(
        file: string,
        args: string[],
        options: {
          name: string;
          cols: number;
          rows: number;
          cwd: string;
          env: Record<string, string>;
        },
      ): IPty;
    };

    const shellArgs = this.getLoginShellArgs(requestedShell);
    const term = pty.spawn(requestedShell, shellArgs, {
      name: "xterm-256color",
      cols: 120,
      rows: 30,
      cwd: requestedCwd,
      env: termEnv,
    });

    // For startup commands, suppress shell init output until the command
    // is injected. For normal terminals, relay immediately.
    let relayEnabled = !opts.startupCommand;
    let skipNextLine = false;

    // Relay PTY output to the renderer.
    term.onData((data: string) => {
      if (!relayEnabled) return;
      if (skipNextLine) {
        skipNextLine = false;
        const newlineIdx = data.indexOf("\n");
        if (newlineIdx >= 0 && newlineIdx < data.length - 1) {
          const remainder = data.slice(newlineIdx + 1);
          const win = this.getMainWindow();
          if (win && !win.isDestroyed()) {
            win.webContents.send("crab:term-data", { id, data: remainder });
          }
          this.writeSessionLog(id, remainder);
        }
        return;
      }
      const win = this.getMainWindow();
      if (win && !win.isDestroyed()) {
        win.webContents.send("crab:term-data", { id, data });
      }
      this.writeSessionLog(id, data);
    });

    const exited = new Promise<void>((resolve) => {
      term.onExit(({ exitCode }) => {
        resolve();
        this.handleSessionExit(id, exitCode);
      });
    });
    this.terminals.set(id, { term, exited, closing: false });

    // If a startup command is specified, wait for the shell to be ready
    // before injecting the command.
    if (opts.startupCommand) {
      const cmd = opts.startupCommand;
      let ready = false;
      let dataCount = 0;
      const readyListener = term.onData((_data: string) => {
        dataCount++;
        if (dataCount >= 1 && !ready) {
          ready = true;
          readyListener.dispose();
          setTimeout(() => {
            skipNextLine = true;
            relayEnabled = true;
            term.write(cmd + "\r");
          }, 30);
        }
      });
      setTimeout(() => {
        if (!ready) {
          ready = true;
          readyListener.dispose();
          skipNextLine = true;
          relayEnabled = true;
          term.write(cmd + "\r");
        }
      }, 2000);
    }

    return { id };
  }

  /** Write data to an active terminal session. */
  handleWrite(_evt: unknown, id: string, data: string): void {
    const session = this.terminals.get(id);
    if (session && !session.closing) session.term.write(data);
  }

  /** Resize an active terminal session. */
  handleResize(_evt: unknown, id: string, cols: number, rows: number): void {
    const session = this.terminals.get(id);
    if (session && !session.closing) session.term.resize(cols, rows);
  }

  /** Kill an active terminal session. */
  handleKill(_evt: unknown, id: string): void {
    const session = this.terminals.get(id);
    if (!session || session.closing) return;
    session.term.kill();
    session.closing = true;
  }

  /**
   * Forward session data to the renderer.
   *
   * Called by TermRouter when it receives data for an in-process session.
   * In the current architecture the bridge's onData handler sends directly,
   * so this method is provided for interface symmetry with DaemonBridge.
   */
  handleSessionData(id: string, data: string): void {
    const win = this.getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:term-data", { id, data });
    }
    this.writeSessionLog(id, data);
  }

  /**
   * Forward session exit to the renderer.
   *
   * Provided for interface symmetry with DaemonBridge. The bridge's own
   * onExit handler already sends the event; this is available for external
   * callers that synthesise exit events (e.g. on daemon disconnect).
   */
  handleSessionExit(id: string, exitCode: number): void {
    this.terminals.delete(id);
    const win = this.getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:term-exit", { id, exitCode });
    }
  }

  /** Close local terminals before Electron tears down their native callbacks. */
  async dispose(): Promise<void> {
    this.shuttingDown = true;
    const exits: Promise<void>[] = [];
    for (const [id, session] of this.terminals) {
      exits.push(session.exited);
      try {
        this.handleKill(null, id);
      } catch {
        // The native exit callback may already be queued.
      }
    }
    if (!exits.length) return;

    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        Promise.all(exits),
        new Promise<void>((resolve) => { timer = setTimeout(resolve, 8_000); }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }

  // -------------------------------------------------------------------------
  // Accessors (for TermRouter / diagnostics)
  // -------------------------------------------------------------------------

  /** Returns the set of active renderer-facing session IDs. */
  getSessionIds(): string[] {
    return [...this.terminals.keys()];
  }

  /** Returns the number of active sessions. */
  get sessionCount(): number {
    return this.terminals.size;
  }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * Convenience factory that mirrors the shape used by the daemon subsystem
 * so `main.ts` can construct both bridges with a uniform call pattern.
 */
export function createInProcessBridge(opts: InProcessBridgeOptions): InProcessBridge {
  return new InProcessBridge(opts);
}
