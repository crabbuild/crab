/**
 * IPC client for communicating with the crab mount coordinator.
 *
 * Connects to the coordinator's Unix socket at `~/.crab/mounts/daemon.sock`,
 * sends newline-delimited JSON requests, and reads JSON responses.
 * Used by the desktop app to manage VFS mounts without shelling out to
 * the CLI for every operation.
 *
 * Protocol: JSON-over-Unix-socket, newline-delimited (matches
 * `crates/crab-vfs/src/ipc_server.rs`).
 */

import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { spawn } from "node:child_process";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Error codes for mount IPC operations. */
export enum MountIpcErrorCode {
  CONNECTION_REFUSED = "CONNECTION_REFUSED",
  TIMEOUT = "TIMEOUT",
  PARSE_ERROR = "PARSE_ERROR",
  OPERATION_FAILED = "OPERATION_FAILED",
  CRAB_NOT_INSTALLED = "CRAB_NOT_INSTALLED",
}

/** Custom error class for mount IPC failures. */
export class MountIpcError extends Error {
  constructor(
    public readonly code: MountIpcErrorCode,
    message: string,
    public readonly cause?: unknown,
  ) {
    super(message);
    this.name = "MountIpcError";
  }
}

/** IPC request sent to the coordinator (mirrors Rust IpcRequest). */
export type IpcRequest =
  | { op: "mount"; remote: string; mountpoint: string; ref: string; read_only: boolean }
  | { op: "unmount"; mountpoint: string }
  | { op: "list" }
  | { op: "status"; mountpoint: string }
  | { op: "refresh"; mountpoint: string }
  | { op: "switch_ref"; mountpoint: string; ref: string };

/** Mount info returned by the `list` operation. */
export interface MountInfo {
  mountpoint: string;
  remote: string;
  ref: string;
  read_only: boolean;
}

/** Detailed mount status returned by the `status` operation. */
export interface MountStatus {
  mountpoint: string;
  remote: string;
  ref: string;
  read_only: boolean;
  head_oid: string | null;
}

/** IPC response from the coordinator (mirrors Rust IpcResponse). */
export interface IpcResponse {
  ok: boolean;
  error?: string;
  mountpoint?: string;
  pid?: number;
  head_oid?: string;
  mounts?: MountInfo[];
  status?: MountStatus;
}

/** Options for mounting a repository. */
export interface MountOptions {
  repo: string;
  mountpoint: string;
  ref?: string;
  readOnly?: boolean;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Default socket path for the coordinator. */
const DEFAULT_SOCKET_PATH = path.join(os.homedir(), ".crab", "mounts", "daemon.sock");

/** Response timeout in milliseconds. */
const RESPONSE_TIMEOUT_MS = 30_000;

/** Backoff delays for coordinator spawn retries (ms). */
const BACKOFF_DELAYS = [100, 200, 400, 800, 1600];

// ---------------------------------------------------------------------------
// MountIpcClient
// ---------------------------------------------------------------------------

/**
 * Client for communicating with the crab mount coordinator via Unix socket.
 *
 * Manages a persistent connection to the coordinator's socket. Sends
 * newline-delimited JSON requests and reads JSON responses.
 */
export class MountIpcClient {
  private socket: net.Socket | null = null;
  private buffer = "";
  private socketPath: string;
  private responseResolve: ((line: string) => void) | null = null;
  private responseReject: ((err: Error) => void) | null = null;

  constructor(socketPath?: string) {
    this.socketPath = socketPath ?? DEFAULT_SOCKET_PATH;
  }

  /**
   * Connect to the coordinator's Unix socket.
   *
   * @throws MountIpcError with CONNECTION_REFUSED if the socket doesn't exist
   *   or the coordinator isn't running.
   */
  async connect(socketPath?: string): Promise<void> {
    if (socketPath) this.socketPath = socketPath;
    if (this.socket) {
      this.disconnect();
    }

    return new Promise<void>((resolve, reject) => {
      const sock = net.createConnection({ path: this.socketPath });

      sock.on("connect", () => {
        this.socket = sock;
        this.buffer = "";
        this.setupDataHandler();
        resolve();
      });

      sock.on("error", (err: NodeJS.ErrnoException) => {
        const code =
          err.code === "ECONNREFUSED" || err.code === "ENOENT"
            ? MountIpcErrorCode.CONNECTION_REFUSED
            : MountIpcErrorCode.CONNECTION_REFUSED;
        reject(new MountIpcError(code, `Cannot connect to coordinator: ${err.message}`, err));
      });
    });
  }

  /** Disconnect from the coordinator socket. */
  disconnect(): void {
    if (this.socket) {
      this.socket.removeAllListeners();
      this.socket.destroy();
      this.socket = null;
    }
    this.buffer = "";
    if (this.responseReject) {
      this.responseReject(new Error("disconnected"));
      this.responseResolve = null;
      this.responseReject = null;
    }
  }

  /** Whether the client is currently connected. */
  get connected(): boolean {
    return this.socket !== null && !this.socket.destroyed;
  }

  /**
   * Send a request to the coordinator and wait for the response.
   *
   * @throws MountIpcError with TIMEOUT if no response within 30s.
   * @throws MountIpcError with PARSE_ERROR if the response is malformed.
   * @throws MountIpcError with OPERATION_FAILED if the coordinator returns an error.
   */
  async send(request: IpcRequest): Promise<IpcResponse> {
    if (!this.socket || this.socket.destroyed) {
      throw new MountIpcError(
        MountIpcErrorCode.CONNECTION_REFUSED,
        "Not connected to coordinator",
      );
    }

    const json = JSON.stringify(request) + "\n";

    // Write the request
    await new Promise<void>((resolve, reject) => {
      this.socket!.write(json, (err) => {
        if (err) reject(new MountIpcError(MountIpcErrorCode.CONNECTION_REFUSED, `Write failed: ${err.message}`, err));
        else resolve();
      });
    });

    // Wait for response with timeout
    const responseLine = await this.waitForLine();

    // Parse the response
    let response: IpcResponse;
    try {
      response = JSON.parse(responseLine) as IpcResponse;
    } catch (err) {
      throw new MountIpcError(
        MountIpcErrorCode.PARSE_ERROR,
        `Malformed response from coordinator: ${responseLine}`,
        err,
      );
    }

    return response;
  }

  private setupDataHandler(): void {
    if (!this.socket) return;

    this.socket.on("data", (chunk: Buffer) => {
      this.buffer += chunk.toString("utf8");
      const newlineIdx = this.buffer.indexOf("\n");
      if (newlineIdx !== -1 && this.responseResolve) {
        const line = this.buffer.slice(0, newlineIdx);
        this.buffer = this.buffer.slice(newlineIdx + 1);
        const resolve = this.responseResolve;
        this.responseResolve = null;
        this.responseReject = null;
        resolve(line);
      }
    });

    this.socket.on("error", (err) => {
      if (this.responseReject) {
        const reject = this.responseReject;
        this.responseResolve = null;
        this.responseReject = null;
        reject(new MountIpcError(MountIpcErrorCode.CONNECTION_REFUSED, err.message, err));
      }
    });

    this.socket.on("close", () => {
      if (this.responseReject) {
        const reject = this.responseReject;
        this.responseResolve = null;
        this.responseReject = null;
        reject(new MountIpcError(MountIpcErrorCode.CONNECTION_REFUSED, "Connection closed"));
      }
    });
  }

  private waitForLine(): Promise<string> {
    // Check if we already have a complete line buffered
    const newlineIdx = this.buffer.indexOf("\n");
    if (newlineIdx !== -1) {
      const line = this.buffer.slice(0, newlineIdx);
      this.buffer = this.buffer.slice(newlineIdx + 1);
      return Promise.resolve(line);
    }

    return new Promise<string>((resolve, reject) => {
      this.responseResolve = resolve;
      this.responseReject = reject;

      // Set up timeout
      const timer = setTimeout(() => {
        if (this.responseReject) {
          const rej = this.responseReject;
          this.responseResolve = null;
          this.responseReject = null;
          rej(new MountIpcError(
            MountIpcErrorCode.TIMEOUT,
            `No response from coordinator within ${RESPONSE_TIMEOUT_MS}ms`,
          ));
        }
      }, RESPONSE_TIMEOUT_MS);

      // Store original resolve to clear timer
      const origResolve = resolve;
      this.responseResolve = (line: string) => {
        clearTimeout(timer);
        origResolve(line);
      };
      const origReject = reject;
      this.responseReject = (err: Error) => {
        clearTimeout(timer);
        origReject(err);
      };
    });
  }
}

// ---------------------------------------------------------------------------
// Coordinator spawning
// ---------------------------------------------------------------------------

/**
 * Resolve the path to the `crab` CLI binary.
 * Checks common locations on macOS/Linux.
 */
function resolveCrabBinary(): string | null {
  const home = os.homedir();
  const candidates = [
    path.join(home, ".cargo", "bin", "crab"),
    "/usr/local/bin/crab",
    "/opt/homebrew/bin/crab",
    path.join(home, ".local", "bin", "crab"),
  ];

  const { existsSync } = require("node:fs") as typeof import("node:fs");
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  return null;
}

/**
 * Spawn the coordinator process if it's not already running.
 *
 * Runs `crab coordinator start` as a detached background process.
 * The coordinator will bind the Unix socket once ready.
 *
 * @throws MountIpcError with CRAB_NOT_INSTALLED if the crab binary cannot be found.
 */
function spawnCoordinator(): void {
  const crabBin = resolveCrabBinary();
  if (!crabBin) {
    throw new MountIpcError(
      MountIpcErrorCode.CRAB_NOT_INSTALLED,
      "crab CLI not found. Install it with `cd crab && make install` or ensure it's in your PATH.",
    );
  }

  const child = spawn(crabBin, ["coordinator", "start"], {
    detached: true,
    stdio: "ignore",
  });

  // Unref so the parent process can exit independently
  child.unref();
}

// ---------------------------------------------------------------------------
// High-level async functions
// ---------------------------------------------------------------------------

/**
 * Get a connected client, spawning the coordinator if needed.
 *
 * Attempts to connect to the socket. If connection is refused, spawns
 * the coordinator and retries with exponential backoff.
 */
async function getConnectedClient(socketPath?: string): Promise<MountIpcClient> {
  const client = new MountIpcClient(socketPath);

  // First attempt
  try {
    await client.connect();
    return client;
  } catch (err) {
    if (err instanceof MountIpcError && err.code === MountIpcErrorCode.CONNECTION_REFUSED) {
      // Coordinator not running — spawn it
    } else {
      throw err;
    }
  }

  // Spawn coordinator
  spawnCoordinator();

  // Retry with backoff
  for (const delay of BACKOFF_DELAYS) {
    await sleep(delay);
    try {
      await client.connect();
      return client;
    } catch (err) {
      if (err instanceof MountIpcError && err.code === MountIpcErrorCode.CONNECTION_REFUSED) {
        continue;
      }
      throw err;
    }
  }

  throw new MountIpcError(
    MountIpcErrorCode.CONNECTION_REFUSED,
    "Coordinator did not become available after spawning. Check that `crab coordinator start` works.",
  );
}

/**
 * Mount a repository via the coordinator.
 *
 * @param opts - Mount options (repo URL/path, mountpoint, optional ref and read-only flag)
 * @returns The IPC response with mountpoint, PID, and HEAD OID on success
 * @throws MountIpcError on connection, timeout, or operation failure
 */
export async function mountRepo(opts: MountOptions): Promise<IpcResponse> {
  const client = await getConnectedClient();
  try {
    const response = await client.send({
      op: "mount",
      remote: opts.repo,
      mountpoint: opts.mountpoint,
      ref: opts.ref ?? "HEAD",
      read_only: opts.readOnly ?? false,
    });
    if (!response.ok) {
      throw new MountIpcError(
        MountIpcErrorCode.OPERATION_FAILED,
        response.error ?? "Mount operation failed",
      );
    }
    return response;
  } finally {
    client.disconnect();
  }
}

/**
 * Unmount a repository via the coordinator.
 *
 * @param mountpoint - The mountpoint path to unmount
 * @throws MountIpcError on connection, timeout, or operation failure
 */
export async function unmountRepo(mountpoint: string): Promise<IpcResponse> {
  const client = await getConnectedClient();
  try {
    const response = await client.send({ op: "unmount", mountpoint });
    if (!response.ok) {
      throw new MountIpcError(
        MountIpcErrorCode.OPERATION_FAILED,
        response.error ?? "Unmount operation failed",
      );
    }
    return response;
  } finally {
    client.disconnect();
  }
}

/**
 * List all active mounts managed by the coordinator.
 *
 * @returns Array of mount info objects
 * @throws MountIpcError on connection or timeout
 */
export async function listMounts(): Promise<MountInfo[]> {
  const client = await getConnectedClient();
  try {
    const response = await client.send({ op: "list" });
    if (!response.ok) {
      throw new MountIpcError(
        MountIpcErrorCode.OPERATION_FAILED,
        response.error ?? "List operation failed",
      );
    }
    return response.mounts ?? [];
  } finally {
    client.disconnect();
  }
}

/**
 * Get the status of a specific mount.
 *
 * @param mountpoint - The mountpoint path to query
 * @returns Detailed mount status
 * @throws MountIpcError on connection, timeout, or if mount not found
 */
export async function getMountStatus(mountpoint: string): Promise<MountStatus> {
  const client = await getConnectedClient();
  try {
    const response = await client.send({ op: "status", mountpoint });
    if (!response.ok) {
      throw new MountIpcError(
        MountIpcErrorCode.OPERATION_FAILED,
        response.error ?? "Status operation failed",
      );
    }
    if (!response.status) {
      throw new MountIpcError(
        MountIpcErrorCode.PARSE_ERROR,
        "Status response missing status field",
      );
    }
    return response.status;
  } finally {
    client.disconnect();
  }
}

/**
 * Refresh a mount (re-fetch from remote, rebuild snapshot).
 *
 * @param mountpoint - The mountpoint path to refresh
 * @returns The IPC response with new HEAD OID
 * @throws MountIpcError on connection, timeout, or operation failure
 */
export async function refreshMount(mountpoint: string): Promise<IpcResponse> {
  const client = await getConnectedClient();
  try {
    const response = await client.send({ op: "refresh", mountpoint });
    if (!response.ok) {
      throw new MountIpcError(
        MountIpcErrorCode.OPERATION_FAILED,
        response.error ?? "Refresh operation failed",
      );
    }
    return response;
  } finally {
    client.disconnect();
  }
}

/**
 * Switch a mount to a different branch/ref.
 *
 * @param mountpoint - The mountpoint path to switch
 * @param ref - The new branch or tag to switch to
 * @returns The IPC response with new HEAD OID
 * @throws MountIpcError on connection, timeout, or operation failure
 */
export async function switchMountRef(mountpoint: string, ref: string): Promise<IpcResponse> {
  const client = await getConnectedClient();
  try {
    const response = await client.send({ op: "switch_ref", mountpoint, ref });
    if (!response.ok) {
      throw new MountIpcError(
        MountIpcErrorCode.OPERATION_FAILED,
        response.error ?? "Switch ref operation failed",
      );
    }
    return response;
  } finally {
    client.disconnect();
  }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
