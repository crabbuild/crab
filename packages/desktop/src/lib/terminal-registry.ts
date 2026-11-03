/**
 * Global registry of active/recent terminal sessions.
 *
 * Terminal components (TerminalView, orchestrator TerminalTab)
 * register their session with us when they spawn and keep the rolling output
 * buffer up to date. The composer's @ context picker then reads from this
 * registry so users can attach "the output from terminal X" without manually
 * copying/pasting.
 *
 * Buffers are capped at a modest size per session to keep the renderer heap
 * predictable — attaching a full multi-MB terminal log would blow up the
 * agent's prompt anyway.
 */

// ── Types ───────────────────────────────────────────────────────────

export interface TerminalSession {
  /** pty id returned by `termSpawn` — stable for the life of the session. */
  id: string;
  /** Human-readable label ("Repo terminal", "Run: npm test", etc.). */
  label: string;
  /** Working directory the session was launched in. */
  cwd?: string;
  /** Rolling buffer, trimmed to MAX_BUFFER_CHARS. */
  buffer: string;
  /** Unix ms timestamp of the most recent output chunk. */
  lastActivityAt: number;
  createdAt: number;
  /** True while the underlying PTY is still alive. */
  active: boolean;
}

// ── State ──────────────────────────────────────────────────────────

const MAX_BUFFER_CHARS = 64 * 1024;
const MAX_RETAINED_SESSIONS = 12;

const sessions = new Map<string, TerminalSession>();
type Listener = (sessions: TerminalSession[]) => void;
const listeners = new Set<Listener>();

function snapshot(): TerminalSession[] {
  // Most-recent-first. Active sessions win ties.
  return Array.from(sessions.values()).sort((a, b) => {
    if (a.active !== b.active) return a.active ? -1 : 1;
    return b.lastActivityAt - a.lastActivityAt;
  });
}

function notify(): void {
  const snap = snapshot();
  for (const listener of listeners) listener(snap);
}

function trimRegistry(): void {
  if (sessions.size <= MAX_RETAINED_SESSIONS) return;
  // Drop the oldest inactive sessions first.
  const ordered = Array.from(sessions.values())
    .filter((s) => !s.active)
    .sort((a, b) => a.lastActivityAt - b.lastActivityAt);
  while (sessions.size > MAX_RETAINED_SESSIONS && ordered.length) {
    const drop = ordered.shift();
    if (drop) sessions.delete(drop.id);
  }
}

// ── Public API ─────────────────────────────────────────────────────

/** Register a new terminal session when it spawns. */
export function registerTerminal(input: {
  id: string;
  label: string;
  cwd?: string;
}): void {
  const now = Date.now();
  sessions.set(input.id, {
    id: input.id,
    label: input.label,
    cwd: input.cwd,
    buffer: "",
    lastActivityAt: now,
    createdAt: now,
    active: true,
  });
  trimRegistry();
  notify();
}

/** Append a chunk of output to a session's buffer. */
export function appendOutput(id: string, chunk: string): void {
  const session = sessions.get(id);
  if (!session) return;
  const next = session.buffer + chunk;
  session.buffer = next.length > MAX_BUFFER_CHARS
    ? next.slice(-MAX_BUFFER_CHARS)
    : next;
  session.lastActivityAt = Date.now();
  notify();
}

/** Mark a session as terminated. Buffer is preserved until it's GC'd. */
export function markTerminalClosed(id: string): void {
  const session = sessions.get(id);
  if (!session) return;
  session.active = false;
  notify();
}

/** Remove a session entirely. */
export function removeTerminal(id: string): void {
  if (sessions.delete(id)) notify();
}

export function getTerminal(id: string): TerminalSession | null {
  return sessions.get(id) ?? null;
}

export function listTerminals(): TerminalSession[] {
  return snapshot();
}

/**
 * Subscribe to registry updates. Returns an unsubscribe function.
 * The listener is called immediately with the current snapshot.
 */
export function subscribe(listener: Listener): () => void {
  listeners.add(listener);
  listener(snapshot());
  return () => {
    listeners.delete(listener);
  };
}

/** Visible tail of a session's buffer, capped for preview use. */
export function terminalTail(id: string, chars = 2000): string {
  const session = sessions.get(id);
  if (!session) return "";
  return session.buffer.length > chars
    ? session.buffer.slice(-chars)
    : session.buffer;
}
