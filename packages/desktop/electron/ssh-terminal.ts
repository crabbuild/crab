/**
 * Remote terminal management over SSH.
 *
 * Manages interactive shell channels (PTY-allocated) over existing SSH
 * connections. Each terminal gets a unique ID and supports bidirectional
 * data flow, resize, and session persistence across reconnections.
 */

import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "./ssh-manager";

// ── Constants ────────────────────────────────────────────────────

/** Timeout (ms) for probing whether a shell channel is still alive. */
const PROBE_TIMEOUT_MS = 2000;

// ── Types ────────────────────────────────────────────────────────

export interface RemoteTerminalSession {
  terminalId: string;
  profileId: string;
  channel: ClientChannel | null;
  cols: number;
  rows: number;
  cwd?: string;
  alive: boolean;
  /** Set to true when the session has ended and is awaiting user action. */
  awaitingReconnect: boolean;
}

export interface RemoteTerminalManagerEvents {
  "ssh:terminal-data": (payload: { terminalId: string; data: string }) => void;
  "ssh:terminal-closed": (payload: { terminalId: string; profileId: string }) => void;
  "ssh:terminal-session-ended": (payload: { terminalId: string; profileId: string }) => void;
  "ssh:terminal-resumed": (payload: { terminalId: string; profileId: string }) => void;
}

// ── Manager ──────────────────────────────────────────────────────

/**
 * Manages interactive SSH shell channels. Each terminal is a PTY-allocated
 * shell channel on an existing SSH connection.
 *
 * Events:
 * - "ssh:terminal-data" ({ terminalId, data }) — output from the remote shell
 * - "ssh:terminal-closed" ({ terminalId, profileId }) — terminal explicitly closed
 * - "ssh:terminal-session-ended" ({ terminalId, profileId }) — channel died unexpectedly
 * - "ssh:terminal-resumed" ({ terminalId, profileId }) — session resumed after reconnection
 */
export class RemoteTerminalManager extends EventEmitter {
  private sessions = new Map<string, RemoteTerminalSession>();

  constructor(private sessionManager: SshSessionManager) {
    super();
    this.attachReconnectionListener();
  }

  /**
   * Listen for SSH connection status changes. When a connection transitions
   * back to "connected", probe all terminal sessions for that profile to
   * determine if their shell channels are still alive.
   */
  private attachReconnectionListener(): void {
    this.sessionManager.on("ssh:status-changed", (payload: {
      profileId: string;
      status: string;
      error?: string;
    }) => {
      if (payload.status === "connected") {
        void this.handleReconnection(payload.profileId);
      }
    });
  }

  /**
   * After SSH reconnection succeeds for a profile, probe each terminal
   * session to determine if the remote shell is still alive.
   *
   * - If alive: resume seamlessly (no user-visible interruption).
   * - If dead: emit "ssh:terminal-session-ended" so the renderer can
   *   show "Session ended" and offer to open a new terminal.
   */
  async handleReconnection(profileId: string): Promise<void> {
    const sessions = this.getSessionsForProfile(profileId);
    if (sessions.length === 0) return;

    const probeResults = await Promise.allSettled(
      sessions.map((session) => this.probeTerminalSession(session)),
    );

    for (let i = 0; i < sessions.length; i++) {
      const session = sessions[i];
      const result = probeResults[i];
      const isAlive = result.status === "fulfilled" && result.value === true;

      if (isAlive) {
        // Shell is still alive — resume seamlessly
        this.emit("ssh:terminal-resumed", {
          terminalId: session.terminalId,
          profileId: session.profileId,
        });
      } else if (!session.awaitingReconnect) {
        // Shell is dead and wasn't already marked by the channel close handler.
        // Mark session and notify renderer.
        session.alive = false;
        session.channel = null;
        session.awaitingReconnect = true;
        this.emit("ssh:terminal-session-ended", {
          terminalId: session.terminalId,
          profileId: session.profileId,
        });
      }
      // If session.awaitingReconnect is already true, the channel close
      // handler already emitted the event — no need to emit again.
    }
  }

  /**
   * Probe a terminal session's shell channel to detect if it's still alive.
   * Sends a null byte (\x00) and waits for any response within the timeout.
   *
   * Returns `true` if the shell responds, `false` if it's dead or unresponsive.
   */
  async probeTerminalSession(session: RemoteTerminalSession): Promise<boolean> {
    const { channel } = session;

    // If the channel reference is already null, the session is dead
    if (!channel || !session.alive) {
      return false;
    }

    return new Promise<boolean>((resolve) => {
      const timeout = setTimeout(() => {
        cleanup();
        resolve(false);
      }, PROBE_TIMEOUT_MS);

      function onData() {
        cleanup();
        resolve(true);
      }

      function onClose() {
        cleanup();
        resolve(false);
      }

      function onError() {
        cleanup();
        resolve(false);
      }

      function cleanup() {
        clearTimeout(timeout);
        channel!.removeListener("data", onData);
        channel!.removeListener("close", onClose);
        channel!.removeListener("error", onError);
      }

      channel.on("data", onData);
      channel.on("close", onClose);
      channel.on("error", onError);

      // Send a null byte as a probe — most shells ignore it but the
      // channel will error or close if the remote process is gone.
      try {
        channel.write("\x00");
      } catch {
        cleanup();
        resolve(false);
      }
    });
  }

  /**
   * Open a new interactive shell channel on the given SSH connection.
   * Allocates a PTY with xterm-256color and the specified dimensions.
   *
   * If `cwd` is provided, sends `cd <cwd>` as the first command after
   * the shell is ready.
   *
   * Returns the terminal ID for subsequent operations.
   */
  async openTerminal(
    profileId: string,
    cols: number,
    rows: number,
    cwd?: string,
  ): Promise<string> {
    const connection = this.sessionManager.getConnection(profileId);
    if (!connection || connection.status !== "connected") {
      throw new Error(
        `Cannot open terminal: SSH connection for profile "${profileId}" is not active.`,
      );
    }

    const { client } = connection;
    const terminalId = randomUUID();

    const channel = await new Promise<ClientChannel>((resolve, reject) => {
      client.shell(
        { term: "xterm-256color", cols, rows },
        (err, stream) => {
          if (err) reject(err);
          else resolve(stream);
        },
      );
    });

    const session: RemoteTerminalSession = {
      terminalId,
      profileId,
      channel,
      cols,
      rows,
      cwd,
      alive: true,
      awaitingReconnect: false,
    };

    this.sessions.set(terminalId, session);

    // Forward shell output to listeners
    channel.on("data", (data: Buffer) => {
      this.emit("ssh:terminal-data", { terminalId, data: data.toString("utf-8") });
    });

    // Handle channel close (unexpected death or remote exit)
    channel.on("close", () => {
      const s = this.sessions.get(terminalId);
      if (s) {
        s.alive = false;
        s.channel = null;
        s.awaitingReconnect = true;
        this.emit("ssh:terminal-session-ended", { terminalId, profileId });
      }
    });

    channel.on("error", () => {
      const s = this.sessions.get(terminalId);
      if (s) {
        s.alive = false;
        s.channel = null;
      }
    });

    // If a working directory was specified, cd into it
    if (cwd) {
      channel.write(`cd ${this.shellEscape(cwd)}\n`);
    }

    return terminalId;
  }

  /**
   * Write data (user input) to the terminal's SSH channel.
   *
   * If the session is in "awaiting reconnect" state (dead shell, user
   * pressed Enter), this triggers a new terminal session instead of
   * writing to the dead channel.
   */
  writeToTerminal(terminalId: string, data: string): void {
    const session = this.sessions.get(terminalId);
    if (!session) {
      throw new Error(`Terminal not found: ${terminalId}`);
    }

    // If the session is awaiting reconnect and user presses Enter,
    // trigger reconnection (open a new shell on the same connection).
    if (session.awaitingReconnect) {
      if (data === "\r" || data === "\n") {
        void this.reconnectTerminal(terminalId);
      }
      // Swallow all other input while awaiting reconnect
      return;
    }

    if (!session.channel || !session.alive) {
      throw new Error(`Terminal session is no longer active: ${terminalId}`);
    }
    session.channel.write(data);
  }

  /**
   * Resize the terminal's PTY dimensions. Sends SIGWINCH equivalent
   * to the remote shell.
   */
  resizeTerminal(terminalId: string, cols: number, rows: number): void {
    const session = this.sessions.get(terminalId);
    if (!session) {
      throw new Error(`Terminal not found: ${terminalId}`);
    }
    if (!session.channel || !session.alive) {
      return; // Silently ignore resize on dead sessions
    }
    session.channel.setWindow(rows, cols, 0, 0);
    session.cols = cols;
    session.rows = rows;
  }

  /**
   * Close a terminal session. Ends the SSH channel and removes the
   * session from the map.
   */
  closeTerminal(terminalId: string): void {
    const session = this.sessions.get(terminalId);
    if (!session) {
      return; // Already closed
    }

    if (session.channel) {
      try {
        session.channel.end();
      } catch {
        // Channel may already be closed
      }
    }

    session.alive = false;
    session.channel = null;
    this.sessions.delete(terminalId);
    this.emit("ssh:terminal-closed", { terminalId, profileId: session.profileId });
  }

  /**
   * Attempt to reconnect a dead terminal session. Opens a new shell
   * channel on the same profile with the same dimensions and cwd.
   *
   * Returns the same terminal ID on success (the session is reused).
   * Throws if the SSH connection is not available.
   */
  async reconnectTerminal(terminalId: string): Promise<string> {
    const session = this.sessions.get(terminalId);
    if (!session) {
      throw new Error(`Terminal not found: ${terminalId}`);
    }

    const connection = this.sessionManager.getConnection(session.profileId);
    if (!connection || connection.status !== "connected") {
      throw new Error(
        `Cannot reconnect terminal: SSH connection for profile "${session.profileId}" is not active.`,
      );
    }

    const { client } = connection;

    const channel = await new Promise<ClientChannel>((resolve, reject) => {
      client.shell(
        { term: "xterm-256color", cols: session.cols, rows: session.rows },
        (err, stream) => {
          if (err) reject(err);
          else resolve(stream);
        },
      );
    });

    // Update the session with the new channel
    session.channel = channel;
    session.alive = true;
    session.awaitingReconnect = false;

    // Re-attach data forwarding
    channel.on("data", (data: Buffer) => {
      this.emit("ssh:terminal-data", { terminalId, data: data.toString("utf-8") });
    });

    channel.on("close", () => {
      const s = this.sessions.get(terminalId);
      if (s) {
        s.alive = false;
        s.channel = null;
        s.awaitingReconnect = true;
        this.emit("ssh:terminal-session-ended", { terminalId, profileId: session.profileId });
      }
    });

    channel.on("error", () => {
      const s = this.sessions.get(terminalId);
      if (s) {
        s.alive = false;
        s.channel = null;
      }
    });

    // cd back to the original working directory
    if (session.cwd) {
      channel.write(`cd ${this.shellEscape(session.cwd)}\n`);
    }

    return terminalId;
  }

  /**
   * Get a terminal session by ID.
   */
  getSession(terminalId: string): RemoteTerminalSession | undefined {
    return this.sessions.get(terminalId);
  }

  /**
   * Get all terminal sessions for a given profile.
   */
  getSessionsForProfile(profileId: string): RemoteTerminalSession[] {
    const result: RemoteTerminalSession[] = [];
    for (const session of this.sessions.values()) {
      if (session.profileId === profileId) {
        result.push(session);
      }
    }
    return result;
  }

  /**
   * Close all terminals for a given profile (e.g., on disconnect).
   */
  closeAllForProfile(profileId: string): void {
    for (const session of this.sessions.values()) {
      if (session.profileId === profileId) {
        this.closeTerminal(session.terminalId);
      }
    }
  }

  /**
   * Escape a path for safe use in a shell command.
   */
  private shellEscape(s: string): string {
    return `'${s.replace(/'/g, "'\\''")}'`;
  }
}
