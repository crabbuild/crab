/**
 * fd-Handoff — transfer PTY master file descriptors from old daemon to new
 * daemon during upgrade.
 *
 * The stdio layout for the successor process:
 *   [0] ignore (stdin)
 *   [1] inherit (stdout — for log piping)
 *   [2] inherit (stderr)
 *   [3] 'ipc' (Node IPC channel for ack/nak)
 *   [4..N+3] PTY master fds (one per live session)
 *
 * serializeSessions() captures session state including the fd index.
 * writeSnapshot(path, data) writes serialized state to a file.
 * adoptSnapshot(snapshot) rebuilds TerminalHost from inherited fds.
 */

import * as fs from "fs";
import * as path from "path";
import type { AgentMeta } from "./types";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Serialized session entry for fd-handoff. */
export interface HandoffSessionEntry {
  /** Session ID. */
  id: string;
  /** PID of the shell process. */
  pid: number;
  /** Session metadata (dimensions, cwd, agent info). */
  meta: {
    agentMeta?: AgentMeta;
    /** The cwd the session originally spawned/adopted with. */
    initialCwd?: string | null;
    /** The latest cwd reported by shell integration, when known. */
    cwd: string | null;
    cols: number;
    rows: number;
  };
  /** Position in the successor's stdio array (fd index starting at 4). */
  fdIndex: number;
  /** Pending output buffer (base64-encoded for JSON safety). */
  buffer: string;
}

/** The full snapshot written to disk for the successor to read. */
export interface HandoffSnapshot {
  /** Protocol version for forward compatibility. */
  version: number;
  /** Timestamp when the snapshot was created. */
  createdAt: number;
  /** Serialized session entries. */
  sessions: HandoffSessionEntry[];
}

// ---------------------------------------------------------------------------
// Session info input (what the caller provides)
// ---------------------------------------------------------------------------

/** Input for serializeSessions — the caller provides live session data. */
export interface LiveSessionInfo {
  id: string;
  pid: number;
  initialCwd?: string | null;
  cwd: string | null;
  cols: number;
  rows: number;
  agentMeta?: AgentMeta;
  /** Pending output that hasn't been consumed by a client yet. */
  pendingBuffer?: Buffer;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** The fd offset where PTY master fds start in the successor's stdio. */
const FD_OFFSET = 4;

/** Current handoff snapshot version. */
const SNAPSHOT_VERSION = 1;

// ---------------------------------------------------------------------------
// serializeSessions
// ---------------------------------------------------------------------------

/**
 * Serialize live sessions into handoff entries with fd indices.
 *
 * Each session is assigned an fdIndex starting at FD_OFFSET (4), corresponding
 * to its position in the successor process's stdio array.
 */
export function serializeSessions(
  sessions: LiveSessionInfo[],
): HandoffSessionEntry[] {
  return sessions.map((session, index) => ({
    id: session.id,
    pid: session.pid,
    meta: {
      agentMeta: session.agentMeta,
      initialCwd: session.initialCwd ?? session.cwd,
      cwd: session.cwd,
      cols: session.cols,
      rows: session.rows,
    },
    fdIndex: FD_OFFSET + index,
    buffer: session.pendingBuffer
      ? session.pendingBuffer.toString("base64")
      : "",
  }));
}

// ---------------------------------------------------------------------------
// writeSnapshot
// ---------------------------------------------------------------------------

/**
 * Write a handoff snapshot to disk atomically (tmp + rename).
 *
 * The snapshot file is written to a temporary path first, then renamed
 * to the target path to prevent partial reads by the successor.
 */
export function writeSnapshot(
  snapshotPath: string,
  sessions: HandoffSessionEntry[],
): void {
  const snapshot: HandoffSnapshot = {
    version: SNAPSHOT_VERSION,
    createdAt: Date.now(),
    sessions,
  };

  const data = JSON.stringify(snapshot);
  const dir = path.dirname(snapshotPath);

  // Ensure directory exists
  fs.mkdirSync(dir, { recursive: true });

  // Atomic write: tmp file + rename
  const tmpPath = `${snapshotPath}.tmp.${process.pid}`;
  fs.writeFileSync(tmpPath, data, { mode: 0o600 });
  fs.renameSync(tmpPath, snapshotPath);
}

// ---------------------------------------------------------------------------
// readSnapshot (helper for adoptSnapshot)
// ---------------------------------------------------------------------------

/**
 * Read and parse a handoff snapshot from disk.
 */
export function readSnapshot(snapshotPath: string): HandoffSnapshot {
  const raw = fs.readFileSync(snapshotPath, "utf-8");
  const snapshot = JSON.parse(raw) as HandoffSnapshot;

  if (snapshot.version !== SNAPSHOT_VERSION) {
    throw new Error(
      `Unsupported handoff snapshot version: ${snapshot.version} (expected ${SNAPSHOT_VERSION})`,
    );
  }

  return snapshot;
}

// ---------------------------------------------------------------------------
// adoptSnapshot
// ---------------------------------------------------------------------------

/**
 * Adopted session info returned after rebuilding from a snapshot.
 */
export interface AdoptedSession {
  id: string;
  pid: number;
  meta: HandoffSessionEntry["meta"];
  fd: number;
  buffer: Buffer;
}

/**
 * Rebuild session state from a handoff snapshot.
 *
 * This function validates the snapshot entries and resolves each session's
 * inherited fd from the current process's stdio. The caller (daemon-main)
 * uses the returned AdoptedSession list to reconstruct TerminalHost sessions
 * by wrapping each fd in a SubprocessHandle.
 *
 * The actual fd is obtained from the process stdio at the position indicated
 * by fdIndex. In the successor process, these fds were passed via the spawn
 * options and are accessible as file descriptors.
 */
export function adoptSnapshot(snapshot: HandoffSnapshot): AdoptedSession[] {
  if (snapshot.version !== SNAPSHOT_VERSION) {
    throw new Error(
      `Unsupported handoff snapshot version: ${snapshot.version} (expected ${SNAPSHOT_VERSION})`,
    );
  }

  return snapshot.sessions.map((entry) => {
    if (entry.fdIndex < FD_OFFSET) {
      throw new Error(
        `Invalid fdIndex ${entry.fdIndex} for session ${entry.id}: must be >= ${FD_OFFSET}`,
      );
    }

    return {
      id: entry.id,
      pid: entry.pid,
      meta: entry.meta,
      fd: entry.fdIndex,
      buffer: entry.buffer ? Buffer.from(entry.buffer, "base64") : Buffer.alloc(0),
    };
  });
}

// ---------------------------------------------------------------------------
// Exports for testing
// ---------------------------------------------------------------------------

export { FD_OFFSET, SNAPSHOT_VERSION };
