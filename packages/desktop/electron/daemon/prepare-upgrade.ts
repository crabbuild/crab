/**
 * Prepare-Upgrade Protocol — zero-downtime daemon binary swap.
 *
 * Flow:
 * 1. Predecessor receives `prepare-upgrade` request
 * 2. Predecessor serializes sessions, writes snapshot to disk
 * 3. Predecessor spawns successor with PTY master fds in stdio 4..N+3
 * 4. Successor detects `--handoff`, reads snapshot, adopts sessions
 * 5. Successor sends `upgrade-ack` via IPC
 * 6. On ack: predecessor exits, successor binds socket with retry
 * 7. On nak/timeout (5s): predecessor continues, kills successor
 *
 * Successor stdio layout:
 *   [0] ignore (stdin)
 *   [1] inherit (stdout — for log piping)
 *   [2] inherit (stderr)
 *   [3] 'ipc' (Node IPC channel for ack/nak)
 *   [4..N+3] PTY master fds (one per live session)
 */

import * as child_process from "node:child_process";
import * as path from "node:path";

import {
  serializeSessions,
  writeSnapshot,
  type LiveSessionInfo,
} from "./fd-handoff";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Timeout for waiting for upgrade-ack from successor (5 seconds). */
const UPGRADE_ACK_TIMEOUT_MS = 5000;

/** Maximum retries for socket binding in successor. */
const SOCKET_BIND_MAX_RETRIES = 10;

/** Delay between socket bind retries (ms). */
const SOCKET_BIND_RETRY_DELAY_MS = 100;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Options for initiating a prepare-upgrade. */
export interface PrepareUpgradeOptions {
  /** Path to the successor daemon binary (Node.js script). */
  successorPath: string;
  /** Socket path the successor should bind to. */
  socketPath: string;
  /** Live sessions to hand off. */
  sessions: LiveSessionInfo[];
  /** PTY master file descriptors, one per session (in session order). */
  ptyFds: number[];
  /** Directory for writing the handoff snapshot file. */
  snapshotDir: string;
  /** Called when the upgrade succeeds (predecessor should exit). */
  onSuccess?: () => void;
  /** Called when the upgrade fails (predecessor continues). */
  onFailure?: (error: string) => void;
  /** Override the fork function for testing. */
  forkFn?: typeof child_process.fork;
  /** Override the ack timeout for testing. */
  ackTimeoutMs?: number;
}

/** Result of the prepare-upgrade operation. */
export interface PrepareUpgradeResult {
  success: boolean;
  error?: string;
}

/** IPC message from successor to predecessor. */
interface SuccessorMessage {
  type: "upgrade-ack" | "upgrade-nak";
  error?: string;
}

// ---------------------------------------------------------------------------
// prepareUpgrade
// ---------------------------------------------------------------------------

/**
 * Initiate the upgrade protocol from the predecessor daemon.
 *
 * Serializes session state, spawns the successor with PTY fds inherited
 * via stdio, and waits for an upgrade-ack (or timeout/nak).
 */
export async function prepareUpgrade(
  opts: PrepareUpgradeOptions,
): Promise<PrepareUpgradeResult> {
  const {
    successorPath,
    socketPath,
    sessions,
    ptyFds,
    snapshotDir,
    onSuccess,
    onFailure,
    forkFn = child_process.fork,
    ackTimeoutMs = UPGRADE_ACK_TIMEOUT_MS,
  } = opts;

  // Validate inputs
  if (sessions.length !== ptyFds.length) {
    const error = `Session count (${sessions.length}) does not match fd count (${ptyFds.length})`;
    onFailure?.(error);
    return { success: false, error };
  }

  // Step 1: Serialize sessions and write snapshot
  const snapshotPath = path.join(snapshotDir, "handoff-snapshot.json");
  let entries;
  try {
    entries = serializeSessions(sessions);
    writeSnapshot(snapshotPath, entries);
  } catch (err) {
    const error = `Failed to write handoff snapshot: ${(err as Error).message}`;
    onFailure?.(error);
    return { success: false, error };
  }

  // Step 2: Build stdio array for successor
  // [0] ignore, [1] inherit, [2] inherit, [3] 'ipc', [4..N+3] PTY fds
  const stdio: Array<"ignore" | "inherit" | "ipc" | number> = [
    "ignore",
    "inherit",
    "inherit",
    "ipc",
    ...ptyFds,
  ];

  // Step 3: Spawn successor process
  let successor: child_process.ChildProcess;
  try {
    successor = forkFn(successorPath, [
      "--socket", socketPath,
      "--handoff",
      "--snapshot", snapshotPath,
    ], {
      stdio: stdio as child_process.StdioOptions,
      detached: true,
      env: {
        ...process.env,
        ELECTRON_RUN_AS_NODE: "1",
      },
    });
  } catch (err) {
    const error = `Failed to spawn successor: ${(err as Error).message}`;
    onFailure?.(error);
    return { success: false, error };
  }

  // Step 4: Wait for upgrade-ack, upgrade-nak, or timeout
  const result = await waitForAck(successor, ackTimeoutMs);

  if (result.success) {
    // Successor acknowledged — predecessor should exit
    successor.unref();
    if (successor.channel) {
      successor.disconnect();
    }
    onSuccess?.();
    return { success: true };
  }

  // Upgrade failed — kill successor, predecessor continues
  killSuccessor(successor);
  onFailure?.(result.error ?? "Unknown upgrade failure");
  return { success: false, error: result.error };
}

// ---------------------------------------------------------------------------
// waitForAck
// ---------------------------------------------------------------------------

/**
 * Wait for the successor to send an upgrade-ack or upgrade-nak via IPC,
 * with a timeout.
 */
function waitForAck(
  successor: child_process.ChildProcess,
  timeoutMs: number,
): Promise<PrepareUpgradeResult> {
  return new Promise((resolve) => {
    let settled = false;

    const timeout = setTimeout(() => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve({ success: false, error: "Upgrade timed out waiting for successor ack" });
    }, timeoutMs);

    const onMessage = (msg: unknown) => {
      if (settled) return;
      const message = msg as SuccessorMessage;

      if (message?.type === "upgrade-ack") {
        settled = true;
        cleanup();
        resolve({ success: true });
      } else if (message?.type === "upgrade-nak") {
        settled = true;
        cleanup();
        resolve({
          success: false,
          error: message.error ?? "Successor sent upgrade-nak",
        });
      }
      // Ignore unrecognized messages
    };

    const onError = (err: Error) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve({ success: false, error: `Successor process error: ${err.message}` });
    };

    const onExit = (code: number | null) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve({
        success: false,
        error: `Successor exited prematurely with code ${code}`,
      });
    };

    function cleanup() {
      clearTimeout(timeout);
      successor.removeListener("message", onMessage);
      successor.removeListener("error", onError);
      successor.removeListener("exit", onExit);
    }

    successor.on("message", onMessage);
    successor.on("error", onError);
    successor.on("exit", onExit);
  });
}

// ---------------------------------------------------------------------------
// killSuccessor
// ---------------------------------------------------------------------------

/**
 * Kill the successor process on upgrade failure.
 */
function killSuccessor(successor: child_process.ChildProcess): void {
  try {
    if (successor.pid && !successor.killed) {
      successor.kill("SIGKILL");
    }
  } catch {
    // Process may have already exited
  }
}

// ---------------------------------------------------------------------------
// Successor-side helpers
// ---------------------------------------------------------------------------

/**
 * Send upgrade-ack to the predecessor via IPC.
 * Called by the successor after successfully adopting sessions.
 */
export function sendUpgradeAck(): void {
  if (process.send) {
    process.send({ type: "upgrade-ack" });
  }
}

/**
 * Send upgrade-nak to the predecessor via IPC.
 * Called by the successor if adoption fails.
 */
export function sendUpgradeNak(error: string): void {
  if (process.send) {
    process.send({ type: "upgrade-nak", error });
  }
}

/**
 * Attempt to bind a socket with retries.
 *
 * The predecessor may still hold the socket briefly after receiving the ack.
 * The successor retries binding until the socket is available.
 */
export async function bindSocketWithRetry(
  bindFn: () => Promise<void>,
  maxRetries: number = SOCKET_BIND_MAX_RETRIES,
  retryDelayMs: number = SOCKET_BIND_RETRY_DELAY_MS,
): Promise<void> {
  let lastError: Error | null = null;

  for (let attempt = 0; attempt <= maxRetries; attempt++) {
    try {
      await bindFn();
      return;
    } catch (err) {
      lastError = err as Error;
      if (attempt < maxRetries) {
        await sleep(retryDelayMs);
      }
    }
  }

  throw new Error(
    `Failed to bind socket after ${maxRetries + 1} attempts: ${lastError?.message}`,
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ---------------------------------------------------------------------------
// Exports for testing
// ---------------------------------------------------------------------------

export {
  UPGRADE_ACK_TIMEOUT_MS,
  SOCKET_BIND_MAX_RETRIES,
  SOCKET_BIND_RETRY_DELAY_MS,
};
