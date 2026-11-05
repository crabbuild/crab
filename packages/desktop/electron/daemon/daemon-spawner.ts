/**
 * DaemonSpawner — handles daemon process lifecycle: probe, spawn,
 * health-check, stale socket cleanup.
 *
 * The spawner is used by DaemonClient to ensure a daemon is running
 * before establishing control/stream connections.
 */

import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import { fork, spawn, type ForkOptions } from "node:child_process";

import {
PROTOCOL_VERSION,
encodeFrame,
FrameDecoder,
type HelloMessage,
type HelloResponse,
type DaemonRequest,
type DaemonResponse,
} from "./protocol";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** macOS limits Unix socket paths to 104 bytes (sun_path in sockaddr_un). */
const MACOS_SOCKET_PATH_LIMIT = 104;

/** Linux limit is 108 bytes. */
const LINUX_SOCKET_PATH_LIMIT = 108;

/** Timeout for socket probe (connect attempt). */
const PROBE_TIMEOUT_MS = 1000;

/** Timeout for health check (hello + ping exchange). */
const HEALTH_CHECK_TIMEOUT_MS = 2000;

/** Timeout waiting for daemon `ready` IPC message after spawn. */
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
    const target = socketPath ?? this.socketPath;

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
    const target = socketPath ?? this.socketPath;
    const authToken = token ?? this.readToken();

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
   * Waits for the `ready` IPC message (10s timeout), then unrefs the child.
   *
   * @throws Error if the daemon fails to start or doesn't send `ready` in time.
   */
  async spawn(entryPath?: string, socketPath?: string): Promise<void> {
    const entry = entryPath ?? this.entryPath;
    const target = socketPath ?? this.socketPath;

    // Validate socket path length
    this.validateSocketPathLength(target);

    // Ensure socket directory exists
    const dir = path.dirname(target);
    fs.mkdirSync(dir, { recursive: true });

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
    safeUnlink(target);
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
   * 3. If healthy → reuse (return)
   * 4. If not healthy or not listening → cleanupStaleSocket() → spawn()
   */
  async ensureRunning(): Promise<void> {
    const listening = await this.probe();

    if (listening) {
      const healthy = await this.healthCheck();
      if (healthy) {
        // Daemon is alive and responsive — reuse it
        return;
      }
    }

    // Daemon is not running or not healthy — clean up and spawn
    this.cleanupStaleSocket();
    await this.spawn();
  }

  // -------------------------------------------------------------------------
  // Socket path validation
  // -------------------------------------------------------------------------

  /**
   * Validate that the socket path doesn't exceed the OS limit.
   * macOS: 104 bytes, Linux: 108 bytes.
   *
   * @throws Error if the path is too long.
   */
  validateSocketPathLength(socketPath: string): void {
    const pathBytes = Buffer.byteLength(socketPath, "utf-8");
    const limit =
      process.platform === "darwin"
        ? MACOS_SOCKET_PATH_LIMIT
        : LINUX_SOCKET_PATH_LIMIT;

    if (pathBytes > limit) {
      throw new Error(
        `Socket path exceeds ${limit}-byte limit for ${process.platform}: ` +
          `${pathBytes} bytes ("${socketPath}")`,
      );
    }
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private readToken(): string | null {
    try {
      return fs.readFileSync(this.tokenPath, "utf-8").trim();
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
