import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { DaemonClient, type DaemonClientOptions, type DaemonApi } from "./daemon-client";
import { readPidFile } from "./path-mismatch-detection";
import { PROTOCOL_VERSION } from "./protocol";
import type { DaemonMetricsSnapshot } from "./daemon-metrics";
import type { SessionInfo } from "./types";

/** Control files belong to an app runtime; replacing a bundle must not strand new spawns. */
export function daemonDirectory(root: string, entryPath: string): string {
  const identity = process.platform === "win32" ? path.resolve(entryPath).toLowerCase() : path.resolve(entryPath);
  return path.join(root, "runtimes", createHash("sha256").update(identity).digest("hex").slice(0, 16));
}

/** Shared configuration stays at the shipped control root across runtime replacements. */
export function daemonControlRoot(directory: string): string {
  return path.basename(path.dirname(directory)) === "runtimes" && /^[a-f0-9]{16}$/.test(path.basename(directory))
    ? path.dirname(path.dirname(directory)) : directory;
}

function directories(root: string): string[] {
  const runtimes = path.join(root, "runtimes");
  const result = [root];
  if (fs.existsSync(runtimes)) {
    for (const entry of fs.readdirSync(runtimes, { withFileTypes: true })) {
      if (entry.isDirectory() && /^[a-f0-9]{16}$/.test(entry.name)) result.push(path.join(runtimes, entry.name));
    }
  }
  return result;
}

function isRunning(directory: string): boolean {
  const metadata = readPidFile(path.join(directory, `daemon-v${PROTOCOL_VERSION}.pid`));
  if (!metadata) return false;
  try { process.kill(metadata.pid, 0); return true; } catch { return false; }
}

/** Claim checkpoints from dead owners by atomic directory moves; live owners retain their history. */
export function prepareDaemonHistory(directory: string): string | undefined {
  const root = daemonControlRoot(directory);
  if (directory === root) return;
  const destination = path.join(directory, "history");
  fs.mkdirSync(destination, { recursive: true });
  for (const previous of directories(root)) {
    if (previous === directory || isRunning(previous)) continue;
    // The tagged alpha.1 root owner used this global history location.
    const source = previous === root
      ? root === path.join(os.homedir(), ".crab", "desktop", "daemon")
        ? path.join(os.homedir(), ".crab", "desktop", "terminal-history") : null
      : path.join(previous, "history");
    if (!source || !fs.existsSync(source)) continue;
    const pending = [source];
    while (pending.length) {
      const parent = pending.pop()!;
      for (const entry of fs.readdirSync(parent, { withFileTypes: true })) {
        if (!entry.isDirectory()) continue;
        const session = path.join(parent, entry.name);
        // Repository-owned IDs contain path separators. Claim session leaves,
        // not their shared parent, so two dead owners' histories can merge.
        if (!fs.existsSync(path.join(session, "meta.json"))) { pending.push(session); continue; }
        const target = path.join(destination, path.relative(source, session));
        // A session already recreated by this owner has newer history. Retain
        // the old checkpoint on disk rather than overwrite the current record.
        if (fs.existsSync(target)) continue;
        fs.mkdirSync(path.dirname(target), { recursive: true });
        try { fs.renameSync(session, target); }
        catch (error) {
          // Another starting runtime may already have claimed this checkpoint.
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
        }
      }
    }
  }
  return destination;
}

/** Route terminal operations to their owning daemon while new sessions use this app's runtime. */
export class DaemonSessions implements DaemonApi {
  private readonly current: DaemonClient;
  private readonly clients = new Map<string, DaemonClient>();
  private readonly owners = new Map<string, DaemonClient>();
  private discovery: Promise<void> | null = null;

  constructor(private readonly options: DaemonClientOptions & {
    controlRoot: string;
    onSessionsDisconnected: (sessionIds: string[]) => void;
    log: (level: "info" | "warn" | "error", message: string) => void;
  }) {
    this.current = new DaemonClient(options);
    this.clients.set(options.socketDir, this.current);
  }

  get isConnected(): boolean { return this.current.isConnected; }

  ownsCurrentSession(sessionId: string): boolean {
    return this.owners.get(sessionId) === this.current;
  }

  async ensureConnected(): Promise<void> {
    await this.current.ensureConnected();
    await this.discover();
  }

  async request<T>(type: string, payload?: unknown, timeoutMs?: number): Promise<T> {
    if (type === "listSessions") return await this.listSessions() as T;
    if (type === "getMetrics") {
      await this.listSessions();
      const metrics = await Promise.all([...this.clients.values()].map(client => client.request<DaemonMetricsSnapshot>(type)));
      const result = { ...metrics[0] };
      for (const metric of metrics.slice(1)) {
        for (const key of Object.keys(result) as (keyof DaemonMetricsSnapshot)[]) {
          if (key === "lastClientConnectedAt") result[key] = Math.max(result[key] ?? 0, metric[key] ?? 0) || null;
          else if (key === "uptimeMs") result[key] = Math.max(result[key], metric[key]);
          else if (key === "fdCount" && (result[key] < 0 || metric[key] < 0)) result[key] = -1;
          else result[key] += metric[key];
        }
      }
      return result as T;
    }
    const sessionId = this.sessionId(payload);
    if (type === "createOrAttach") {
      const sessions = await this.listSessions();
      // A saved shell that ended on an old owner must reopen on the current
      // runtime; its previous helper may have disappeared with the app bundle.
      if (sessionId && !sessions.some(session => session.sessionId === sessionId && session.isAlive)) this.owners.delete(sessionId);
    }
    const client = sessionId ? this.owners.get(sessionId) ?? this.current : this.current;
    try {
      const result = await client.request<T>(type, payload, timeoutMs);
      if (sessionId && type === "createOrAttach") this.owners.set(sessionId, client);
      return result;
    } catch (error) {
      // Only current-owner disconnects may trigger AutoRespawn. A previous
      // owner's failure must not synthesize exits for this runtime's terminals.
      if (client !== this.current && !client.isConnected) throw new Error("Previous terminal owner is unavailable", { cause: error });
      throw error;
    }
  }

  async notify(type: string, payload?: unknown): Promise<void> {
    const sessionId = this.sessionId(payload);
    const client = sessionId ? this.owners.get(sessionId) ?? this.current : this.current;
    try { await client.notify(type, payload); }
    catch (error) {
      if (client !== this.current && !client.isConnected) throw new Error("Previous terminal owner is unavailable", { cause: error });
      throw error;
    }
  }

  async getAgentSessions(): Promise<SessionInfo[]> {
    return (await this.listSessions()).filter(session => session.agentMeta !== undefined);
  }

  mapSessionId(rendererId: string, daemonId: string): void { this.current.mapSessionId(rendererId, daemonId); }
  getDaemonSessionId(rendererId: string): string | undefined { return this.current.getDaemonSessionId(rendererId); }
  unmapSessionId(rendererId: string): void { this.current.unmapSessionId(rendererId); }

  disconnect(): void {
    for (const client of this.clients.values()) client.disconnect();
  }

  private sessionId(payload: unknown): string | undefined {
    if (typeof payload !== "object" || payload === null || !("sessionId" in payload)) return;
    return typeof payload.sessionId === "string" ? payload.sessionId : undefined;
  }

  private async listSessions(): Promise<SessionInfo[]> {
    await this.ensureConnected();
    const sessions: SessionInfo[] = [];
    for (const [directory, client] of this.clients) {
      try {
        const owned = await client.request<SessionInfo[]>("listSessions");
        for (const session of owned) this.owners.set(session.sessionId, client);
        sessions.push(...owned);
      } catch (error) {
        if (client === this.current) throw error;
        this.removePeer(directory, client);
        this.options.log("warn", `Previous terminal daemon unavailable: ${error instanceof Error ? error.message : String(error)}`);
      }
    }
    return sessions;
  }

  private async discover(): Promise<void> {
    if (this.discovery) return this.discovery;
    this.discovery = this.discoverPeers();
    try { await this.discovery; } finally { this.discovery = null; }
  }

  private async discoverPeers(): Promise<void> {
    const { controlRoot } = this.options;
    // v2.0.0-alpha.1 put its control files directly in the root. Keep that
    // owner reachable until its live terminals end; it never receives new spawns.
    for (const directory of directories(controlRoot)) {
      if (this.clients.has(directory)) continue;
      if (!isRunning(directory)) continue;
      const client = new DaemonClient({
        ...this.options,
        socketDir: directory,
        onDisconnected: () => this.removePeer(directory, client),
        onColdRestoreAvailable: (sessionId, payload) => {
          this.owners.set(sessionId, client);
          this.options.onColdRestoreAvailable?.(sessionId, payload);
        },
      });
      try {
        await client.ensureConnected();
        this.clients.set(directory, client);
      } catch (error) {
        client.disconnect();
        this.options.log("warn", `Previous terminal daemon could not connect: ${error instanceof Error ? error.message : String(error)}`);
      }
    }
  }

  private removePeer(directory: string, client: DaemonClient): void {
    client.disconnect();
    this.clients.delete(directory);
    const lost: string[] = [];
    for (const [sessionId, owner] of this.owners) {
      if (owner !== client) continue;
      this.owners.delete(sessionId);
      lost.push(sessionId);
    }
    this.options.onSessionsDisconnected(lost);
  }
}
