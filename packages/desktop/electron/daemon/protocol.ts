/**
 * Wire protocol: binary framing, message types, and error classes
 * for the PTY daemon.
 *
 * Frame format:
 *   [u32 BE totalLen][u32 BE jsonLen][JSON header (UTF-8)][optional binary payload]
 *
 * totalLen = 4 (jsonLen field) + jsonLen + payloadLen
 * The first 4 bytes (totalLen itself) are NOT included in totalLen.
 */

import type {
AgentActivityState,
AgentMeta,ShellReadyState,
TerminalSnapshot
} from "./types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Incremented on breaking wire-shape changes. */
export const PROTOCOL_VERSION = 2;

/** Request IDs prefixed with this are fire-and-forget (no response). */
export const NOTIFY_PREFIX = "notify_";

/** Maximum frame size: 8 MB. Frames exceeding this are rejected. */
export const MAX_FRAME_SIZE = 8 * 1024 * 1024;

/** Header size: 4 bytes totalLen + 4 bytes jsonLen. */
const HEADER_SIZE = 8;

// ---------------------------------------------------------------------------
// Message Types
// ---------------------------------------------------------------------------

/** First message on each socket connection. */
export interface HelloMessage {
  type: "hello";
  version: number;
  token: string;
  clientId: string;
  role: "control" | "stream";
}

/** Daemon response to a hello message. */
export interface HelloResponse {
  type: "hello";
  ok: boolean;
  error?: string;
}

/** Payload for createOrAttach requests. */
export interface CreateOrAttachPayload {
  sessionId: string;
  cols: number;
  rows: number;
  cwd?: string;
  env?: Record<string, string>;
  command?: string;
  shell?: string;
  startupCommand?: string;
  shellReadySupported?: boolean;
  agentMeta?: AgentMeta;
}

/** Result of a createOrAttach operation. */
export interface CreateOrAttachResult {
  isNew: boolean;
  snapshot: TerminalSnapshot | null;
  pid: number | null;
  shellState: ShellReadyState;
}

/** Compose a snapshot for replay into an xterm-compatible terminal. */
export function snapshotReplayData(snapshot: Pick<TerminalSnapshot, "snapshotAnsi" | "rehydrateSequences">): string {
  const alternateScreen = "\x1b[?1049h";
  const modes = snapshot.rehydrateSequences;
  if (!modes.includes(alternateScreen)) return snapshot.snapshotAnsi + modes;
  // Entering the alternate screen clears it. Paint after entering, then
  // restore remaining modes without clearing the recovered agent display.
  return alternateScreen + snapshot.snapshotAnsi + modes.replaceAll(alternateScreen, "");
}

/** Payload for prepare-upgrade requests. */
export interface PrepareUpgradePayload {
  /** Path to the new daemon binary to spawn as successor. */
  successorPath: string;
}

/** Result of a prepare-upgrade operation. */
export interface PrepareUpgradeResult {
  /** Whether the upgrade completed successfully. */
  success: boolean;
  /** Error message if the upgrade failed. */
  error?: string;
}

/** Payload for cold-restore-resume requests. */
export interface ColdRestoreResumePayload {
  /** The session ID to resume (must match a cold-restore-available event). */
  sessionId: string;
  /** User-provided env values to merge with the sanitized persisted env. */
  env?: Record<string, string>;
}

/** Result of a cold-restore-resume operation. */
export interface ColdRestoreResumeResult {
  /** Whether the cwd from the checkpoint was found on disk. */
  cwdMissing?: boolean;
  /** The cwd actually used (may be os.homedir() if original was missing). */
  resolvedCwd: string;
  pid: number | null;
}

/** Union of all client → daemon requests on the control socket. */
export type DaemonRequest =
  | { id: string; type: "createOrAttach"; payload: CreateOrAttachPayload }
  | { id: string; type: "write"; payload: { sessionId: string } }
  | { id: string; type: "resize"; payload: { sessionId: string; cols: number; rows: number } }
  | { id: string; type: "detach"; payload: { sessionId: string } }
  | { id: string; type: "kill"; payload: { sessionId: string } }
  | { id: string; type: "signal"; payload: { sessionId: string; signal: string } }
  | { id: string; type: "getCwd"; payload: { sessionId: string } }
  | { id: string; type: "getSnapshot"; payload: { sessionId: string } }
  | { id: string; type: "clearScrollback"; payload: { sessionId: string } }
  | { id: string; type: "listSessions" }
  | { id: string; type: "ping" }
  | { id: string; type: "getMetrics" }
  | { id: string; type: "getDaemonInfo" }
  | { id: string; type: "shutdown"; payload: { killSessions: boolean } }
  | { id: string; type: "prepare-upgrade"; payload: PrepareUpgradePayload }
  | { id: string; type: "checkpoint"; payload: { sessionId: string } }
  | { id: string; type: "cold-restore-resume"; payload: ColdRestoreResumePayload }
  | { id: string; type: "cold-restore-discard"; payload: { sessionId: string } };

/** Daemon → client response on the control socket. */
export type DaemonResponse =
  | { id: string; ok: true; payload: unknown }
  | { id: string; ok: false; error: string };

/** Payload for the cold-restore-available event. */
export interface ColdRestorePayload {
  snapshot: TerminalSnapshot;
  /** Unix timestamp (ms) when the checkpoint was written. */
  savedAt: number;
  /** Env keys that were stripped at save time (secrets). */
  sanitizedKeys: string[];
  cwd: string | null;
  cols: number;
  rows: number;
}

/** Daemon → client events on the stream socket. */
export type DaemonEvent =
  | { type: "event"; event: "data"; sessionId: string }
  | { type: "event"; event: "exit"; sessionId: string; payload: { code: number } }
  | {
      type: "event";
      event: "state-changed";
      sessionId: string;
      payload: {
        state: AgentActivityState;
        previousState: AgentActivityState;
        timestamp: number;
      };
    }
  | {
      type: "event";
      event: "cold-restore-available";
      sessionId: string;
      payload: ColdRestorePayload;
    }
  | {
      type: "event";
      event: "cold-restore-failed";
      sessionId: string;
      payload: { reason: string };
    };

// ---------------------------------------------------------------------------
// Ping response with health metadata
// ---------------------------------------------------------------------------

export interface PingResponse {
  uptime: number;
  sessions: number;
  clients: number;
  memoryUsage: number;
  protocolVersion: number;
  fdCount: number;
  /** Older v2 daemons omit this; only advertised idle shutdown is safe to automate. */
  idleShutdownSupported?: boolean;
}

// ---------------------------------------------------------------------------
// Metrics response
// ---------------------------------------------------------------------------

export interface MetricsResponse {
  uptimeMs: number;
  totalSessionsSpawned: number;
  activeSessionCount: number;
  peakSessionCount: number;
  totalBytesRelayed: number;
  totalCheckpointsWritten: number;
  checkpointFailures: number;
  clientConnections: number;
  clientDisconnections: number;
  respawnCount: number;
  lastClientConnectedAt: number | null;
  memoryUsageBytes: number;
  fdCount: number;
}

// ---------------------------------------------------------------------------
// Error Classes
// ---------------------------------------------------------------------------

/** Protocol-level error (malformed frame, version mismatch, etc.). */
export class DaemonProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DaemonProtocolError";
  }
}

/** Raised when a request references a non-existent session. */
export class SessionNotFoundError extends Error {
  readonly sessionId: string;

  constructor(sessionId: string) {
    super(`Session not found: ${sessionId}`);
    this.name = "SessionNotFoundError";
    this.sessionId = sessionId;
  }
}

// ---------------------------------------------------------------------------
// Frame Encoding
// ---------------------------------------------------------------------------

/**
 * Encode a JSON header and optional binary payload into a framed buffer.
 *
 * Layout: [u32 BE totalLen][u32 BE jsonLen][JSON (UTF-8)][payload]
 * totalLen = 4 + jsonLen + payload.length
 */
export function encodeFrame(json: unknown, payload?: Buffer | Uint8Array): Buffer {
  const jsonBytes = Buffer.from(JSON.stringify(json), "utf-8");
  const jsonLen = jsonBytes.length;
  const payloadLen = payload ? payload.length : 0;
  const totalLen = 4 + jsonLen + payloadLen;

  const frame = Buffer.allocUnsafe(HEADER_SIZE + jsonLen + payloadLen);

  // Write totalLen (u32 BE)
  frame.writeUInt32BE(totalLen, 0);
  // Write jsonLen (u32 BE)
  frame.writeUInt32BE(jsonLen, 4);
  // Write JSON bytes
  jsonBytes.copy(frame, HEADER_SIZE);
  // Write payload if present
  if (payload && payloadLen > 0) {
    if (Buffer.isBuffer(payload)) {
      payload.copy(frame, HEADER_SIZE + jsonLen);
    } else {
      frame.set(payload, HEADER_SIZE + jsonLen);
    }
  }

  return frame;
}

// ---------------------------------------------------------------------------
// Frame Decoding
// ---------------------------------------------------------------------------

/** A decoded frame with its JSON header and optional binary payload. */
export interface DecodedFrame {
  json: unknown;
  payload: Buffer | null;
}

/**
 * Streaming frame decoder that handles split chunks and buffering.
 *
 * Feed data via `push(chunk)` and retrieve complete frames via `frames()`.
 */
export class FrameDecoder {
  private buffer: Buffer = Buffer.alloc(0);
  private pending: DecodedFrame[] = [];

  /**
   * Push a chunk of data into the decoder. Complete frames are parsed
   * and made available via `frames()`.
   *
   * @throws DaemonProtocolError if a frame exceeds MAX_FRAME_SIZE.
   */
  push(chunk: Buffer): void {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    this.drain();
  }

  /** Retrieve and clear all fully decoded frames since the last call. */
  frames(): DecodedFrame[] {
    const result = this.pending;
    this.pending = [];
    return result;
  }

  /** Number of buffered bytes not yet forming a complete frame. */
  get bufferedBytes(): number {
    return this.buffer.length;
  }

  private drain(): void {
    while (this.buffer.length >= HEADER_SIZE) {
      const totalLen = this.buffer.readUInt32BE(0);

      // Reject oversized frames
      if (totalLen > MAX_FRAME_SIZE) {
        throw new DaemonProtocolError(
          `Frame size ${totalLen} exceeds maximum ${MAX_FRAME_SIZE}`,
        );
      }

      // Not enough data for the full frame yet
      const frameSize = 4 + totalLen; // 4 bytes for totalLen field + totalLen
      if (this.buffer.length < frameSize) {
        break;
      }

      const jsonLen = this.buffer.readUInt32BE(4);
      const jsonStart = HEADER_SIZE;
      const jsonEnd = jsonStart + jsonLen;
      const payloadStart = jsonEnd;
      const payloadEnd = frameSize;

      // Parse JSON header
      const jsonStr = this.buffer.toString("utf-8", jsonStart, jsonEnd);
      let json: unknown;
      try {
        json = JSON.parse(jsonStr);
      } catch (e) {
        throw new DaemonProtocolError(`Invalid JSON in frame: ${(e as Error).message}`);
      }

      // Extract payload (may be empty)
      const payloadLen = payloadEnd - payloadStart;
      const payload = payloadLen > 0 ? this.buffer.subarray(payloadStart, payloadEnd) : null;

      this.pending.push({
        json,
        payload: payload ? Buffer.from(payload) : null,
      });

      // Advance buffer past this frame
      this.buffer = this.buffer.subarray(frameSize);
    }
  }
}
