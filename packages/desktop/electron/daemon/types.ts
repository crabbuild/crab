/**
 * Shared type definitions for the PTY daemon protocol.
 */

/** Lifecycle state of a PTY session. */
export type SessionState = "created" | "spawning" | "running" | "exiting" | "exited";

/** Whether the shell has finished initialization. */
export type ShellReadyState = "pending" | "ready" | "timed_out" | "unsupported";

/** Terminal mode flags tracked by the headless emulator. */
export interface TerminalModes {
  bracketedPaste: boolean;
  mouseTracking: boolean;
  mouseTrackingMode: "none" | "x10" | "vt200" | "drag" | "any";
  sgrMouseMode: boolean;
  applicationCursor: boolean;
  alternateScreen: boolean;
}

/** Serialized terminal state for reattach and checkpoints. */
export interface TerminalSnapshot {
  snapshotAnsi: string;
  scrollbackAnsi: string;
  rehydrateSequences: string;
  cwd: string | null;
  modes: TerminalModes;
  cols: number;
  rows: number;
  scrollbackLines: number;
}

/** Activity state of an agent session as reported via OSC sequences. */
export type AgentActivityState = "working" | "waiting" | "idle" | "exited";

/** Metadata attached to agent sessions. */
export interface AgentMeta {
  taskId: string;
  agentType: string;
  worktreePath: string;
}

/** Summary info for a session returned by listSessions. */
export interface SessionInfo {
  sessionId: string;
  state: SessionState;
  shellState: ShellReadyState;
  isAlive: boolean;
  pid: number | null;
  /** The cwd used when the session was created. Stable even after `cd`. */
  initialCwd?: string | null;
  /** Current cwd reported by OSC-7, when available. */
  cwd: string | null;
  cols: number;
  rows: number;
  createdAt: number;
  /** Timestamp of the most recent PTY output chunk. Zero until first output. */
  lastDataAt?: number;
  agentMeta?: AgentMeta & {
    activityState: AgentActivityState;
    stateChangedAt: number;
  };
}

/** A connected stream client that receives PTY output. */
export interface StreamClient {
  id: string;
  sendData(sessionId: string, data: Buffer): void;
  sendExit(sessionId: string, code: number): void;
  sendStateChanged(
    sessionId: string,
    state: AgentActivityState,
    previousState: AgentActivityState,
    timestamp: number,
  ): void;
}

/** Abstraction over node-pty for testability. */
export interface SubprocessHandle {
  readonly pid: number;
  /** The PTY master file descriptor (for fd-handoff during upgrades). */
  readonly fd: number;
  write(data: string): void;
  resize(cols: number, rows: number): void;
  kill(signal?: string): void;
  forceKill(): void;
  signal(sig: string): void;
  pause(): void;
  resume(): void;
  onData(cb: (data: string) => void): void;
  onExit(cb: (exitCode: number) => void): void;
  dispose(): void;
}
