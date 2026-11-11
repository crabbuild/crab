/**
 * DaemonSpawner — handles daemon process lifecycle: probe, spawn,
 * health-check, stale socket cleanup.
 *
 * The spawner is used by DaemonClient to ensure a daemon is running
 * before establishing control/stream connections.
 */

import * as net from "node:net";
import * as fs from "node:fs";
import * as diskFs from "original-fs";
import * as path from "node:path";
import { execFile, fork, spawn, type ForkOptions } from "node:child_process";
import { promisify } from "node:util";
import { socketAddress } from "./socket-address";
import { DaemonClient } from "./daemon-client";
import { normalizePath, readPidFile } from "./path-mismatch-detection";

import {
PROTOCOL_VERSION,
encodeFrame,
FrameDecoder,
type HelloMessage,
type HelloResponse,
type DaemonRequest,
type DaemonResponse,
type PingResponse,
} from "./protocol";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Timeout for socket probe (connect attempt). */
const PROBE_TIMEOUT_MS = 1000;

/** Timeout for health check (hello + ping exchange). */
const HEALTH_CHECK_TIMEOUT_MS = 2000;

/** Timeout waiting for daemon readiness. */
const SPAWN_READY_TIMEOUT_MS = 10_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface DaemonSpawnerOptions {
  /** Directory where socket/token/pid files live. */
  socketDir: string;
  /** Path to the daemon entry script (daemon-main.ts/js). */
  entryPath: string;
  /** Protocol version for socket filename. */
  protocolVersion?: number;
}

interface PidFileData {
  pid: number;
  startedAtMs: number;
  entryPath: string;
}

// ---------------------------------------------------------------------------
// DaemonSpawner
// ---------------------------------------------------------------------------

export class DaemonSpawner {
  private readonly socketDir: string;
  private readonly entryPath: string;
  private readonly protocolVersion: number;
  private ensurePromise: Promise<void> | null = null;

  constructor(opts: DaemonSpawnerOptions) {
    this.socketDir = opts.socketDir;
    this.entryPath = opts.entryPath;
    this.protocolVersion = opts.protocolVersion ?? PROTOCOL_VERSION;
  }

  /** Socket path derived from socketDir and protocol version. */
  get socketPath(): string {
    return path.join(this.socketDir, `daemon-v${this.protocolVersion}.sock`);
  }

  /** Token file path. */
  get tokenPath(): string {
    return path.join(this.socketDir, `daemon-v${this.protocolVersion}.token`);
  }

  /** PID file path. */
  get pidPath(): string {
    return path.join(this.socketDir, `daemon-v${this.protocolVersion}.pid`);
  }

  // -------------------------------------------------------------------------
  // probe — check if daemon is listening
  // -------------------------------------------------------------------------

  /**
   * Attempt a TCP connect to the socket with a 1-second timeout.
   * Returns true if the connection succeeds (daemon is listening).
   */
  async probe(socketPath?: string): Promise<boolean> {
    const target = socketAddress(socketPath ?? this.socketPath);

    return new Promise<boolean>((resolve) => {
      const socket = net.createConnection(target);
      const timer = setTimeout(() => {
        socket.destroy();
        resolve(false);
      }, PROBE_TIMEOUT_MS);

      socket.on("connect", () => {
        clearTimeout(timer);
        socket.destroy();
        resolve(true);
      });

      socket.on("error", () => {
        clearTimeout(timer);
        socket.destroy();
        resolve(false);
      });
    });
  }

  // -------------------------------------------------------------------------
  // healthCheck — verify daemon is responsive
  // -------------------------------------------------------------------------

  /**
   * Connect to the daemon, perform a hello handshake + ping, and verify
   * it responds within 2 seconds. Returns true if the daemon is healthy.
   */
  async healthCheck(socketPath?: string, token?: string): Promise<boolean> {
    const target = socketAddress(socketPath ?? this.socketPath);
    const authToken = token ?? this.readToken(socketPath);

    if (!authToken) return false;

    return new Promise<boolean>((resolve) => {
      const socket = net.createConnection(target);
      const decoder = new FrameDecoder();
      let phase: "hello" | "ping" | "done" = "hello";

      const timer = setTimeout(() => {
        socket.destroy();
        resolve(false);
      }, HEALTH_CHECK_TIMEOUT_MS);

      const cleanup = (result: boolean) => {
        clearTimeout(timer);
        socket.destroy();
        resolve(result);
      };

      socket.on("connect", () => {
        // Send hello
        const hello: HelloMessage = {
          type: "hello",
          version: this.protocolVersion,
          token: authToken,
          clientId: "spawner-health-check",
          role: "control",
        };
        socket.write(encodeFrame(hello));
      });

      socket.on("data", (chunk: Buffer) => {
        decoder.push(chunk);
        const frames = decoder.frames();

        for (const frame of frames) {
          if (phase === "hello") {
            const resp = frame.json as HelloResponse;
            if (!resp.ok) {
              cleanup(false);
              return;
            }
            // Hello succeeded, send ping
            phase = "ping";
            const ping: DaemonRequest = { id: "health-ping", type: "ping" };
            socket.write(encodeFrame(ping));
          } else if (phase === "ping") {
            const resp = frame.json as DaemonResponse;
            cleanup(resp.ok === true);
            return;
          }
        }
      });

      socket.on("error", () => {
        cleanup(false);
      });
    });
  }

  // -------------------------------------------------------------------------
  // spawn — fork detached daemon process
  // -------------------------------------------------------------------------

  /**
   * Start the daemon as a detached child with ELECTRON_RUN_AS_NODE=1.
   * Waits for authenticated pipe readiness on Windows and IPC ready on Unix.
   *
   * @throws Error if the daemon fails to start or doesn't send `ready` in time.
   */
  async spawn(entryPath?: string, socketPath?: string): Promise<void> {
    const entry = entryPath ?? this.entryPath;
    const target = socketPath ?? this.socketPath;

    // Ensure socket directory exists
    const dir = path.dirname(target);
    fs.mkdirSync(dir, { recursive: true });

    if (process.platform === "win32") {
      await this.spawnWindows(entry, target);
      return;
    }

    return new Promise<void>((resolve, reject) => {
      const options: ForkOptions = {
        detached: true,
        stdio: ["ignore", "ignore", "ignore", "ipc"],
        env: {
          ...process.env,
          ELECTRON_RUN_AS_NODE: "1",
        },
      };
      // Electron leaves Chromium descriptors inheritable on Linux. Close them
      // before exec so the durable daemon cannot retain browser sockets/files;
      // descriptor 3 is the Node IPC channel used for the ready message.
      const child = process.platform === "linux"
        ? spawn("/bin/bash", [
            "-c",
            'for file in /proc/self/fd/*; do fd=${file##*/}; if [ "$fd" -ge 4 ]; then eval "exec $fd>&-"; fi; done; exec "$@"',
            "crab-pty-daemon",
            process.execPath,
            entry,
            "--socket",
            target,
          ], options)
        : fork(entry, ["--socket", target], options);

      const timer = setTimeout(() => {
        child.removeAllListeners();
        child.kill();
        reject(new Error(`Daemon did not send 'ready' within ${SPAWN_READY_TIMEOUT_MS}ms`));
      }, SPAWN_READY_TIMEOUT_MS);

      child.on("message", (msg: unknown) => {
        if (isReadyMessage(msg)) {
          clearTimeout(timer);
          child.removeAllListeners("error");
          child.removeAllListeners("exit");

          // Disconnect IPC and unref so Electron can exit
          child.disconnect();
          child.unref();

          resolve();
        }
      });

      child.on("error", (err) => {
        clearTimeout(timer);
        child.removeAllListeners();
        reject(new Error(`Failed to spawn daemon: ${err.message}`));
      });

      child.on("exit", (code, signal) => {
        clearTimeout(timer);
        child.removeAllListeners();
        reject(
          new Error(
            `Daemon exited prematurely: code=${code}, signal=${signal}`,
          ),
        );
      });
    });
  }

  // -------------------------------------------------------------------------
  // cleanupStaleSocket — remove stale socket/token/pid files
  // -------------------------------------------------------------------------

  /**
   * Verify PID file and remove stale socket/token/pid files if the
   * recorded process is no longer alive.
   *
   * @returns true if cleanup was performed (files were stale).
   */
  cleanupStaleSocket(socketPath?: string, pidPath?: string): boolean {
    const target = socketPath ?? this.socketPath;
    const pidFile = pidPath ?? this.pidPath;

    // Check PID file
    try {
      const raw = fs.readFileSync(pidFile, "utf-8");
      const pidData = JSON.parse(raw) as PidFileData;

      if (isProcessAlive(pidData.pid)) {
        // Process is alive — don't clean up
        return false;
      }
    } catch {
      // PID file doesn't exist or is malformed — treat as stale
    }

    // Remove stale files
    safeUnlink(socketAddress(target));
    safeUnlink(pidFile);

    // Derive and remove token file
    const tokenFile = target.replace(/\.sock$/, ".token");
    safeUnlink(tokenFile);

    return true;
  }

  // -------------------------------------------------------------------------
  // resetHandle — reset internal state for respawn
  // -------------------------------------------------------------------------

  /**
   * Reset internal daemon process handle. Used when the daemon has died
   * and we need to respawn.
   */
  resetHandle(): void {
  }

  // -------------------------------------------------------------------------
  // ensureRunning — probe → healthCheck → reuse OR cleanup → spawn
  // -------------------------------------------------------------------------

  /**
   * Ensure a healthy daemon is running. Probes the socket, performs a
   * health check if reachable, and spawns a new daemon if needed.
   *
   * Flow:
   * 1. probe() — is anything listening?
   * 2. If yes → healthCheck() — is it responsive?
   * 3. If healthy → retire a missing idle runtime, otherwise reuse
   * 4. If not healthy or not listening → cleanupStaleSocket() → spawn()
   */
  async ensureRunning(): Promise<void> {
    // Startup, reconnect and new-terminal requests share one retirement/spawn
    // transaction; otherwise a disconnect can create two competing owners.
    if (this.ensurePromise) return this.ensurePromise;
    this.ensurePromise = this.ensureRunningOnce();
    try { await this.ensurePromise; }
    finally { this.ensurePromise = null; }
  }

  private async ensureRunningOnce(): Promise<void> {
    const listening = await this.probe();

    if (listening) {
      const healthy = await this.healthCheck();
      if (healthy) {
        if (await this.retireMissingIdleRuntime()) return this.ensureRunningOnce();
        return;
      }
    }

    // Daemon is not running or not healthy — clean up and spawn
    this.cleanupStaleSocket();
    await this.spawn();
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private async retireMissingIdleRuntime(): Promise<boolean> {
    const previous = readPidFile(this.pidPath);
    if (!previous || normalizePath(previous.entryPath) === normalizePath(this.entryPath)) return false;
    // Electron can cache virtual ASAR entries after their bundle is deleted.
    // The physical archive is the availability boundary for packaged runtimes.
    const archive = previous.entryPath.match(/^(.*?\.asar)(?:[/\\]|$)/)?.[1];
    if (diskFs.existsSync(archive ?? previous.entryPath)) return false;
    const client = new DaemonClient({
      socketDir: this.socketDir,
      protocolVersion: this.protocolVersion,
      daemonEntryPath: this.entryPath,
      onSessionData() {}, onSessionExit() {}, onAgentStateChange() {},
    });
    try {
      const health = await client.request<PingResponse>("ping");
      // A responsive daemon may still own live PTYs. Pre-capability v2
      // daemons require an explicit restart; never infer permission to stop them.
      if (!health.idleShutdownSupported || health.sessions !== 0) return false;
      const current = readPidFile(this.pidPath);
      if (current?.pid !== previous.pid) return false;
      const result = await client.request<{ acknowledged: boolean }>("shutdown", { killSessions: false });
      if (!result.acknowledged) return false;
      const deadline = Date.now() + SPAWN_READY_TIMEOUT_MS;
      while (isProcessAlive(previous.pid)) {
        if (Date.now() >= deadline) throw new Error("Terminal service did not finish its idle restart.");
        await new Promise(resolve => setTimeout(resolve, 25));
      }
      return true;
    } finally { client.disconnect(); }
  }

  private async spawnWindows(entry: string, socketPath: string): Promise<void> {
    // libuv inherits Windows handles even with ignored stdio. The system
    // shell starts an independent process, so the durable daemon cannot
    // retain Electron's launch pipes after the application exits.
    const literal = (value: string) => `'${value.replaceAll("'", "''")}'`;
    const args = literal(`"${entry}" --socket "${socketPath}"`);
    const command = `$ErrorActionPreference='Stop'; (Start-Process -FilePath ${literal(process.execPath)} -ArgumentList ${args} -WindowStyle Hidden -PassThru).Id`;
    const powershell = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
    const deadline = Date.now() + SPAWN_READY_TIMEOUT_MS;
    const { stdout } = await promisify(execFile)(powershell, [
      "-NoProfile", "-NonInteractive", "-EncodedCommand", Buffer.from(command, "utf16le").toString("base64"),
    ], {
      windowsHide: true,
      timeout: SPAWN_READY_TIMEOUT_MS,
      env: { ...process.env, ELECTRON_RUN_AS_NODE: "1" },
    });
    const pid = Number(stdout.trim());
    if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error("Daemon launcher did not return a process ID");
    while (Date.now() < deadline) {
      if (!isProcessAlive(pid)) throw new Error("Daemon exited prematurely");
      if (await this.healthCheck(socketPath)) return;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    try { process.kill(pid); } catch { /* The daemon already exited. */ }
    throw new Error(`Daemon pipe did not become ready within ${SPAWN_READY_TIMEOUT_MS}ms`);
  }

  private readToken(socketPath = this.socketPath): string | null {
    try {
      return fs.readFileSync(socketPath.replace(/\.sock$/, ".token"), "utf-8").trim();
    } catch {
      return null;
    }
  }
}

// ---------------------------------------------------------------------------
// Module-level helpers
// ---------------------------------------------------------------------------

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function isReadyMessage(msg: unknown): boolean {
  return (
    typeof msg === "object" &&
    msg !== null &&
    (msg as Record<string, unknown>).type === "ready"
  );
}

function safeUnlink(filePath: string): void {
  try {
    fs.unlinkSync(filePath);
  } catch {
    // File may not exist — that's fine
  }
}
