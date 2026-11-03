/**
 * AgentTracker — OSC state parsing and timeout enforcement for agent sessions.
 *
 * Parses `\x1b]777;crab-state;<state>\x07` from PTY output using partial-match
 * buffering (handles sequences split across data chunks). Maintains per-session
 * AgentActivityState and enforces a configurable timeout (default 30 min) that
 * sends SIGTERM to unattended working agents.
 */

import type { AgentActivityState, AgentMeta } from "./types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** OSC prefix for agent state sequences. */
const OSC_PREFIX = "\x1b]777;crab-state;";

/** OSC terminator (BEL). */
const OSC_TERMINATOR = "\x07";

/** Default agent timeout: 30 minutes. */
const DEFAULT_TIMEOUT_MS = 30 * 60 * 1000;

/** Valid agent activity states (excluding "exited" which is set programmatically). */
const VALID_STATES: ReadonlySet<string> = new Set(["working", "waiting", "idle"]);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface AgentSessionInfo {
  sessionId: string;
  taskId: string;
  agentType: string;
  worktreePath: string;
  state: AgentActivityState;
  stateChangedAt: number;
  exitCode?: number;
}

export interface AgentTrackerOptions {
  /** Timeout in ms for unattended working agents. Default: 30 min. */
  timeoutMs?: number;
  /** Called when timeout fires — should send SIGTERM to the session. */
  onTimeout?: (sessionId: string) => void;
}

// ---------------------------------------------------------------------------
// OSC State Scanner — partial-match buffering
// ---------------------------------------------------------------------------

/**
 * Per-session scanner state for detecting OSC state sequences that may
 * span multiple data chunks.
 */
interface ScannerState {
  /** Current match position within the OSC_PREFIX. */
  prefixPos: number;
  /** Characters held back during a partial prefix match. */
  heldChars: string;
  /** Whether we've matched the full prefix and are now collecting the state value. */
  inStateValue: boolean;
  /** Accumulated state value characters (between prefix and terminator). */
  stateValue: string;
}

function createScannerState(): ScannerState {
  return { prefixPos: 0, heldChars: "", inStateValue: false, stateValue: "" };
}

/**
 * Scan a chunk of PTY output for OSC state sequences.
 * Returns all detected state values (there may be multiple in one chunk).
 * The scanner state is mutated in place for partial-match buffering.
 */
function scanOscStates(scanner: ScannerState, data: string): string[] {
  const states: string[] = [];

  for (let i = 0; i < data.length; i++) {
    const ch = data[i]!;

    if (scanner.inStateValue) {
      // We've matched the prefix, now collecting the state value until BEL
      if (ch === OSC_TERMINATOR) {
        // Complete sequence found
        states.push(scanner.stateValue);
        scanner.stateValue = "";
        scanner.inStateValue = false;
        scanner.prefixPos = 0;
        scanner.heldChars = "";
      } else {
        scanner.stateValue += ch;
        // Safety: if state value gets unreasonably long, abort
        if (scanner.stateValue.length > 64) {
          scanner.inStateValue = false;
          scanner.stateValue = "";
          scanner.prefixPos = 0;
          scanner.heldChars = "";
        }
      }
    } else {
      // Matching the prefix character by character
      if (ch === OSC_PREFIX[scanner.prefixPos]) {
        scanner.heldChars += ch;
        scanner.prefixPos++;

        if (scanner.prefixPos === OSC_PREFIX.length) {
          // Full prefix matched — now collecting state value
          scanner.inStateValue = true;
          scanner.stateValue = "";
          scanner.heldChars = "";
        }
      } else {
        // Mismatch — flush held chars and reset
        scanner.heldChars = "";
        scanner.prefixPos = 0;

        // Check if current char starts a new potential match
        if (ch === OSC_PREFIX[0]) {
          scanner.heldChars = ch;
          scanner.prefixPos = 1;
        }
      }
    }
  }

  return states;
}

// ---------------------------------------------------------------------------
// AgentTracker
// ---------------------------------------------------------------------------

export class AgentTracker {
  private sessions: Map<string, AgentSessionInfo> = new Map();
  private scanners: Map<string, ScannerState> = new Map();
  private timeouts: Map<string, ReturnType<typeof setTimeout>> = new Map();
  private timeoutMs: number;
  private onTimeoutCallback?: (sessionId: string) => void;
  private disposed = false;

  /** Callback invoked when an agent's state changes. */
  onStateChange:
    | ((sessionId: string, state: AgentActivityState, prev: AgentActivityState) => void)
    | null = null;

  constructor(opts: AgentTrackerOptions = {}) {
    this.timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.onTimeoutCallback = opts.onTimeout;
  }

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  /**
   * Register a new agent session for tracking.
   */
  registerSession(sessionId: string, meta: AgentMeta): void {
    if (this.disposed) return;

    const info: AgentSessionInfo = {
      sessionId,
      taskId: meta.taskId,
      agentType: meta.agentType,
      worktreePath: meta.worktreePath,
      state: "idle",
      stateChangedAt: Date.now(),
    };

    this.sessions.set(sessionId, info);
    this.scanners.set(sessionId, createScannerState());
    this.resetTimeout(sessionId);
  }

  /**
   * Process PTY output data for a session. Scans for OSC state sequences
   * and resets the timeout on any output.
   */
  handleData(sessionId: string, data: string): void {
    if (this.disposed) return;

    const info = this.sessions.get(sessionId);
    if (!info) return;

    const scanner = this.scanners.get(sessionId);
    if (!scanner) return;

    // Reset timeout on any output (agent is producing data)
    this.resetTimeout(sessionId);

    // Scan for OSC state sequences
    const detectedStates = scanOscStates(scanner, data);

    for (const stateValue of detectedStates) {
      if (VALID_STATES.has(stateValue)) {
        this.transitionState(sessionId, stateValue as AgentActivityState);
      }
      // Unrecognized states are silently ignored
    }
  }

  /**
   * Handle session exit. Sets state to "exited" and cleans up timeout.
   */
  handleExit(sessionId: string, code: number): void {
    if (this.disposed) return;

    const info = this.sessions.get(sessionId);
    if (!info) return;

    this.clearTimeout(sessionId);
    this.scanners.delete(sessionId);

    const prev = info.state;
    info.state = "exited";
    info.exitCode = code;
    info.stateChangedAt = Date.now();

    if (prev !== "exited") {
      this.onStateChange?.(sessionId, "exited", prev);
    }
  }

  /**
   * Get the current activity state for a session.
   */
  getState(sessionId: string): AgentActivityState | null {
    const info = this.sessions.get(sessionId);
    return info?.state ?? null;
  }

  /**
   * List all tracked agent sessions.
   */
  listAgentSessions(): AgentSessionInfo[] {
    return Array.from(this.sessions.values());
  }

  /**
   * Remove a session from tracking entirely.
   */
  removeSession(sessionId: string): void {
    this.clearTimeout(sessionId);
    this.sessions.delete(sessionId);
    this.scanners.delete(sessionId);
  }

  /**
   * Dispose the tracker: clear all timeouts and state.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    for (const timer of this.timeouts.values()) {
      clearTimeout(timer);
    }
    this.timeouts.clear();
    this.sessions.clear();
    this.scanners.clear();
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private transitionState(sessionId: string, newState: AgentActivityState): void {
    const info = this.sessions.get(sessionId);
    if (!info) return;

    const prev = info.state;
    if (prev === newState) return;
    if (prev === "exited") return; // Cannot transition out of exited

    info.state = newState;
    info.stateChangedAt = Date.now();

    this.onStateChange?.(sessionId, newState, prev);
  }

  private resetTimeout(sessionId: string): void {
    this.clearTimeout(sessionId);

    if (this.timeoutMs <= 0) return;

    const timer = setTimeout(() => {
      this.timeouts.delete(sessionId);
      const info = this.sessions.get(sessionId);
      if (info && info.state === "working") {
        this.onTimeoutCallback?.(sessionId);
      }
    }, this.timeoutMs);

    // Unref so it doesn't keep the process alive
    if (timer.unref) {
      timer.unref();
    }

    this.timeouts.set(sessionId, timer);
  }

  private clearTimeout(sessionId: string): void {
    const timer = this.timeouts.get(sessionId);
    if (timer) {
      clearTimeout(timer);
      this.timeouts.delete(sessionId);
    }
  }
}
