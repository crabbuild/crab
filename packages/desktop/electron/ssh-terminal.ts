import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import type { Client, ClientChannel } from "ssh2";
import type { SshProfile, SshSessionManager } from "./ssh-manager";

const TMUX = "tmux -L crab-desktop -f /dev/null";
const OPERATION_TIMEOUT_MS = 10_000;

export interface RemoteTerminalSession {
  terminalId: string;
  profileId: string;
  channel: ClientChannel | null;
  cols: number;
  rows: number;
  cwd?: string;
  ownerPath?: string;
  alive: boolean;
  awaitingReconnect: boolean;
}

/** Remote PTYs live in tmux; SSH channels only attach clients to them. */
export class RemoteTerminalManager extends EventEmitter {
  private sessions = new Map<string, RemoteTerminalSession>();
  private connecting = new Map<string, Promise<void>>();
  private attaching = new Map<string, Promise<string>>();

  constructor(
    private sessionManager: SshSessionManager,
    private profileForId?: (id: string) => SshProfile | undefined,
  ) {
    super();
    sessionManager.on("ssh:status-changed", ({ profileId, status }: { profileId: string; status: string }) => {
      if (status === "connected") void this.handleReconnection(profileId);
    });
  }

  async handleReconnection(profileId: string): Promise<void> {
    await Promise.allSettled(this.getSessionsForProfile(profileId)
      .filter(session => session.awaitingReconnect)
      .map(session => this.reconnectTerminal(session.terminalId)));
  }

  async openTerminal(profileId: string, cols: number, rows: number, cwd?: string, resumeId?: string, ownerPath?: string): Promise<string> {
    const terminalId = resumeId ?? randomUUID();
    if (!/^[0-9a-f-]{36}$/.test(terminalId)) throw new Error("Invalid remote terminal identity");
    const existing = this.sessions.get(terminalId);
    if (existing && existing.profileId !== profileId) throw new Error("Remote terminal belongs to a different SSH profile");
    const session = existing ?? { terminalId, profileId, cols, rows, cwd, ownerPath, channel: null, alive: false, awaitingReconnect: false };
    this.sessions.set(terminalId, session);
    try {
      return await this.attach(session, resumeId !== undefined);
    } catch (error) {
      session.awaitingReconnect = true;
      if (!resumeId) this.sessions.delete(terminalId);
      throw error;
    }
  }

  private async connectedClient(profileId: string): Promise<Client> {
    const active = this.sessionManager.getConnection(profileId);
    if (active?.status === "connected" || active?.status === "deploying") return active.client;
    let pending = this.connecting.get(profileId);
    if (!pending) {
      const profile = this.profileForId?.(profileId) ?? active?.profile;
      if (!profile) throw new Error("SSH profile is not available. Reconnect the profile and retry.");
      // A profile may already be connecting during app restoration. Wait for
      // that owner instead of disconnecting its transport to start another.
      const connection = active?.status === "connecting" || active?.status === "reconnecting"
        ? new Promise<void>((resolve, reject) => {
          const changed = (event: { profileId: string; status: string; error?: string }) => {
            if (event.profileId !== profileId) return;
            if (event.status === "connected" || event.status === "deploying") finish();
            else if (event.status === "error" || event.status === "disconnected") finish(new Error(event.error ?? "SSH connection ended"));
          };
          const finish = (error?: Error) => {
            clearTimeout(timer);
            this.sessionManager.removeListener("ssh:status-changed", changed);
            if (error) reject(error); else resolve();
          };
          const timer = setTimeout(() => finish(new Error("SSH connection timed out")), profile.connectionTimeoutMs ?? OPERATION_TIMEOUT_MS);
          this.sessionManager.on("ssh:status-changed", changed);
        })
        : this.sessionManager.connect(profile);
      pending = connection.finally(() => this.connecting.delete(profileId));
      this.connecting.set(profileId, pending);
    }
    await pending;
    const connection = this.sessionManager.getConnection(profileId);
    if (connection?.status !== "connected" && connection?.status !== "deploying") throw new Error("SSH connection is not active");
    return connection.client;
  }

  private attach(session: RemoteTerminalSession, resume: boolean): Promise<string> {
    const pending = this.attaching.get(session.terminalId);
    if (pending) return pending;
    const operation = this.attachChannel(session, resume).finally(() => this.attaching.delete(session.terminalId));
    this.attaching.set(session.terminalId, operation);
    return operation;
  }

  private async attachChannel(session: RemoteTerminalSession, resume: boolean): Promise<string> {
    const client = await this.connectedClient(session.profileId);
    const name = `crab-${session.terminalId}`;
    const target = this.shellEscape(`=${name}`);
    const check = "command -v tmux >/dev/null 2>&1 || { printf '%s\\n' 'Durable SSH terminals require tmux on the remote host. Install tmux and retry.' >&2; exit 127; }; ";
    // Restore never creates a replacement shell: that would misrepresent an
    // ended agent as resumed. New tabs alone may create a remote session.
    const prepare = resume
      ? `${TMUX} has-session -t ${target}`
      : `${TMUX} new-session -d -s ${this.shellEscape(name)} -x ${session.cols} -y ${session.rows}${session.cwd ? ` -c ${this.shellEscape(session.cwd)}` : ""} \\; set-option -t ${this.shellEscape(name)} status off \\; set-option -t ${this.shellEscape(name)} prefix None \\; set-option -t ${this.shellEscape(name)} destroy-unattached off`;
    await this.run(client, check + prepare);
    if (this.sessions.get(session.terminalId) !== session) throw new Error("Terminal was closed while connecting");
    const channel = await new Promise<ClientChannel>((resolve, reject) => {
      let settled = false;
      const timer = setTimeout(() => { settled = true; reject(new Error("Remote terminal attachment timed out")); }, OPERATION_TIMEOUT_MS);
      client.exec(`${TMUX} attach-session -d -t ${target}`, { pty: { term: "xterm-256color", cols: session.cols, rows: session.rows } },
        (error, stream) => {
          // SSH can deliver a channel after the request timed out. Release
          // that attachment so a failed tab cannot retain an unseen client.
          if (settled) { stream?.close(); return; }
          settled = true;
          clearTimeout(timer);
          if (error) reject(error); else resolve(stream);
        });
    });
    if (this.sessions.get(session.terminalId) !== session) { channel.end(); throw new Error("Terminal was closed while connecting"); }
    const previous = session.channel;
    session.channel = channel;
    session.alive = true;
    session.awaitingReconnect = false;
    previous?.end();
    channel.on("data", (data: Buffer) => {
      if (session.channel === channel) this.emit("ssh:terminal-data", { terminalId: session.terminalId, data: data.toString("utf8"), ownerPath: session.ownerPath });
    });
    const ended = () => {
      // An old channel may close after its replacement has attached.
      if (this.sessions.get(session.terminalId) !== session || session.channel !== channel) return;
      session.channel = null;
      session.alive = false;
      session.awaitingReconnect = true;
      this.emit("ssh:terminal-session-ended", { terminalId: session.terminalId, profileId: session.profileId });
    };
    channel.on("close", ended);
    channel.on("error", ended);
    channel.stderr.on("data", (data: Buffer) => this.emit("ssh:terminal-data", { terminalId: session.terminalId, data: data.toString("utf8"), ownerPath: session.ownerPath }));
    // tmux configures its terminal before drawing the first screen. Returning
    // earlier can flush user input while the SSH PTY is still being initialized.
    await new Promise<void>((resolve, reject) => {
      const finish = (error?: Error) => {
        clearTimeout(timer);
        channel.removeListener("data", ready);
        channel.removeListener("close", closed);
        channel.removeListener("error", failed);
        if (error) reject(error); else resolve();
      };
      const ready = () => finish();
      const closed = () => finish(new Error("Remote terminal closed before attachment completed"));
      const failed = (error: Error) => finish(error);
      const timer = setTimeout(() => { channel.close(); finish(new Error("Remote terminal attachment timed out")); }, OPERATION_TIMEOUT_MS);
      channel.once("data", ready).once("close", closed).once("error", failed);
    });
    return session.terminalId;
  }

  private run(client: Client, command: string): Promise<void> {
    return new Promise((resolve, reject) => {
      let channel: ClientChannel | undefined;
      let settled = false;
      const finish = (error?: Error) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (error) reject(error); else resolve();
      };
      const timer = setTimeout(() => { channel?.close(); finish(new Error("Remote terminal operation timed out")); }, OPERATION_TIMEOUT_MS);
      client.exec(command, (error, stream) => {
        if (error) { finish(error); return; }
        if (settled) { stream.close(); return; }
        channel = stream;
        let message = "";
        stream.stderr.on("data", (data: Buffer) => { message = (message + data.toString("utf8")).slice(-4096); });
        stream.on("error", finish);
        stream.on("close", (code: number) => finish(code === 0 ? undefined : new Error(message.trim() || "Remote terminal has ended or could not be attached")));
        stream.resume();
      });
    });
  }

  writeToTerminal(terminalId: string, data: string): void {
    const session = this.sessions.get(terminalId);
    if (!session) throw new Error(`Terminal not found: ${terminalId}`);
    if (session.awaitingReconnect) {
      if (data === "\r" || data === "\n") void this.reconnectTerminal(terminalId).catch(() => {});
      return;
    }
    if (!session.channel) throw new Error("Remote terminal is disconnected");
    session.channel.write(data);
  }

  resizeTerminal(terminalId: string, cols: number, rows: number): void {
    const session = this.sessions.get(terminalId);
    if (!session) throw new Error(`Terminal not found: ${terminalId}`);
    session.cols = Math.max(1, Math.floor(cols));
    session.rows = Math.max(1, Math.floor(rows));
    session.channel?.setWindow(session.rows, session.cols, 0, 0);
  }

  /** Release this attachment; the remote shell and foreground process continue. */
  detachTerminal(terminalId: string): void {
    const session = this.sessions.get(terminalId);
    if (!session) return;
    this.sessions.delete(terminalId);
    session.channel?.end();
    session.channel = null;
    session.alive = false;
  }

  /** Explicit user termination destroys only this terminal's remote PTY. */
  async closeTerminal(terminalId: string): Promise<void> {
    const session = this.sessions.get(terminalId);
    if (!session) return;
    const client = await this.connectedClient(session.profileId);
    await this.run(client, `if ${TMUX} has-session -t ${this.shellEscape(`=crab-${terminalId}`)}; then ${TMUX} kill-session -t ${this.shellEscape(`=crab-${terminalId}`)}; fi`);
    this.detachTerminal(terminalId);
    this.emit("ssh:terminal-closed", { terminalId, profileId: session.profileId });
  }

  async reconnectTerminal(terminalId: string): Promise<string> {
    const session = this.sessions.get(terminalId);
    if (!session) throw new Error(`Terminal not found: ${terminalId}`);
    try {
      const id = await this.attach(session, true);
      this.emit("ssh:terminal-resumed", { terminalId: id, profileId: session.profileId });
      return id;
    } catch (error) {
      this.emit("ssh:terminal-data", { terminalId, ownerPath: session.ownerPath, data: `\r\n[Cannot resume remote terminal: ${error instanceof Error ? error.message : String(error)}]\r\n` });
      throw error;
    }
  }

  getSession(terminalId: string): RemoteTerminalSession | undefined { return this.sessions.get(terminalId); }
  getSessionsForProfile(profileId: string): RemoteTerminalSession[] { return [...this.sessions.values()].filter(session => session.profileId === profileId); }
  async closeAllForProfile(profileId: string): Promise<void> { await Promise.all(this.getSessionsForProfile(profileId).map(session => this.closeTerminal(session.terminalId))); }
  private shellEscape(value: string): string { return `'${value.replace(/'/g, "'\\''")}'`; }
}
