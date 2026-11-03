import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { AgentTracker } from "../electron/daemon/agent-tracker";
import type { AgentMeta } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeMeta(overrides: Partial<AgentMeta> = {}): AgentMeta {
  return {
    taskId: "task-1",
    agentType: "claude",
    worktreePath: "/tmp/worktree",
    ...overrides,
  };
}

/** Build an OSC state sequence for the given state value. */
function oscState(state: string): string {
  return `\x1b]777;crab-state;${state}\x07`;
}

// ---------------------------------------------------------------------------
// OSC State Parsing
// ---------------------------------------------------------------------------

describe("AgentTracker — OSC parsing", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("detects a state sequence in a single chunk", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", `output${oscState("working")}more`);

    expect(tracker.getState("s1")).toBe("working");
    tracker.dispose();
  });

  it("detects a state sequence split across two chunks (partial-match buffering)", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    const full = oscState("waiting");
    const splitPoint = Math.floor(full.length / 2);

    tracker.handleData("s1", full.slice(0, splitPoint));
    expect(tracker.getState("s1")).toBe("idle"); // not yet matched

    tracker.handleData("s1", full.slice(splitPoint));
    expect(tracker.getState("s1")).toBe("waiting");
    tracker.dispose();
  });

  it("detects a state sequence split across three chunks", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    const full = oscState("working");
    const split1 = 5;
    const split2 = full.length - 3;

    tracker.handleData("s1", full.slice(0, split1));
    tracker.handleData("s1", full.slice(split1, split2));
    tracker.handleData("s1", full.slice(split2));

    expect(tracker.getState("s1")).toBe("working");
    tracker.dispose();
  });

  it("handles multiple state sequences in a single chunk", () => {
    const tracker = new AgentTracker();
    const stateChanges: string[] = [];
    tracker.onStateChange = (_sid, state) => {
      stateChanges.push(state);
    };
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", `${oscState("working")}output${oscState("waiting")}`);

    expect(tracker.getState("s1")).toBe("waiting");
    expect(stateChanges).toEqual(["working", "waiting"]);
    tracker.dispose();
  });

  it("ignores unrecognized state values", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("unknown-state"));

    expect(tracker.getState("s1")).toBe("idle"); // unchanged
    tracker.dispose();
  });

  it("ignores empty state values", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState(""));

    expect(tracker.getState("s1")).toBe("idle");
    tracker.dispose();
  });

  it("handles data with no OSC sequences", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", "hello world\r\nprompt$ ");

    expect(tracker.getState("s1")).toBe("idle");
    tracker.dispose();
  });

  it("handles a false start (partial prefix followed by mismatch)", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    // Start of prefix then wrong char, followed by real sequence
    tracker.handleData("s1", "\x1b]777;crab-stateX" + oscState("working"));

    expect(tracker.getState("s1")).toBe("working");
    tracker.dispose();
  });

  it("handles OSC sequence at the very start of data", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));

    expect(tracker.getState("s1")).toBe("working");
    tracker.dispose();
  });

  it("handles OSC sequence at the very end of data", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", "output" + oscState("waiting"));

    expect(tracker.getState("s1")).toBe("waiting");
    tracker.dispose();
  });

  it("aborts state value collection if it exceeds 64 chars", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    // Craft a sequence with an absurdly long state value (no terminator)
    const longValue = "a".repeat(100);
    tracker.handleData("s1", `\x1b]777;crab-state;${longValue}`);

    expect(tracker.getState("s1")).toBe("idle"); // not matched
    tracker.dispose();
  });

  it("does not process data for unregistered sessions", () => {
    const tracker = new AgentTracker();

    // Should not throw
    tracker.handleData("unknown", oscState("working"));

    expect(tracker.getState("unknown")).toBeNull();
    tracker.dispose();
  });
});

// ---------------------------------------------------------------------------
// State Transitions
// ---------------------------------------------------------------------------

describe("AgentTracker — state transitions", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("starts in idle state after registration", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    expect(tracker.getState("s1")).toBe("idle");
    tracker.dispose();
  });

  it("transitions from idle to working", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));

    expect(tracker.getState("s1")).toBe("working");
    tracker.dispose();
  });

  it("transitions from working to waiting", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));
    tracker.handleData("s1", oscState("waiting"));

    expect(tracker.getState("s1")).toBe("waiting");
    tracker.dispose();
  });

  it("transitions from waiting to idle", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("waiting"));
    tracker.handleData("s1", oscState("idle"));

    expect(tracker.getState("s1")).toBe("idle");
    tracker.dispose();
  });

  it("emits onStateChange callback on transition", () => {
    const tracker = new AgentTracker();
    const changes: Array<{ sid: string; state: string; prev: string }> = [];
    tracker.onStateChange = (sid, state, prev) => {
      changes.push({ sid, state, prev });
    };
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));
    tracker.handleData("s1", oscState("waiting"));

    expect(changes).toEqual([
      { sid: "s1", state: "working", prev: "idle" },
      { sid: "s1", state: "waiting", prev: "working" },
    ]);
    tracker.dispose();
  });

  it("does not emit onStateChange for same-state transitions", () => {
    const tracker = new AgentTracker();
    const changes: string[] = [];
    tracker.onStateChange = (_sid, state) => {
      changes.push(state);
    };
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));
    tracker.handleData("s1", oscState("working")); // same state

    expect(changes).toEqual(["working"]);
    tracker.dispose();
  });

  it("transitions to exited on handleExit", () => {
    const tracker = new AgentTracker();
    const changes: Array<{ state: string; prev: string }> = [];
    tracker.onStateChange = (_sid, state, prev) => {
      changes.push({ state, prev });
    };
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));
    tracker.handleExit("s1", 0);

    expect(tracker.getState("s1")).toBe("exited");
    expect(changes).toContainEqual({ state: "exited", prev: "working" });
    tracker.dispose();
  });

  it("records exit code on handleExit", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleExit("s1", 42);

    const sessions = tracker.listAgentSessions();
    expect(sessions[0]!.exitCode).toBe(42);
    tracker.dispose();
  });

  it("cannot transition out of exited state", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleExit("s1", 0);
    tracker.handleData("s1", oscState("working"));

    expect(tracker.getState("s1")).toBe("exited");
    tracker.dispose();
  });

  it("does not emit duplicate exited events", () => {
    const tracker = new AgentTracker();
    const changes: string[] = [];
    tracker.onStateChange = (_sid, state) => {
      changes.push(state);
    };
    tracker.registerSession("s1", makeMeta());

    tracker.handleExit("s1", 0);
    tracker.handleExit("s1", 1); // second exit should be ignored

    expect(changes.filter((s) => s === "exited")).toHaveLength(1);
    tracker.dispose();
  });
});

// ---------------------------------------------------------------------------
// Timeout Enforcement
// ---------------------------------------------------------------------------

describe("AgentTracker — timeout enforcement", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("fires timeout callback when agent is in working state", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ timeoutMs: 1000, onTimeout });
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));

    // Advance past the timeout
    vi.advanceTimersByTime(1001);

    expect(onTimeout).toHaveBeenCalledWith("s1");
    tracker.dispose();
  });

  it("does not fire timeout when agent is in waiting state", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ timeoutMs: 1000, onTimeout });
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("waiting"));

    vi.advanceTimersByTime(1001);

    expect(onTimeout).not.toHaveBeenCalled();
    tracker.dispose();
  });

  it("does not fire timeout when agent is in idle state", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ timeoutMs: 1000, onTimeout });
    tracker.registerSession("s1", makeMeta());

    // Agent stays in idle (default)
    vi.advanceTimersByTime(1001);

    expect(onTimeout).not.toHaveBeenCalled();
    tracker.dispose();
  });

  it("resets timeout on any PTY output", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ timeoutMs: 1000, onTimeout });
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));

    // Advance 800ms (not yet timed out)
    vi.advanceTimersByTime(800);
    expect(onTimeout).not.toHaveBeenCalled();

    // More output resets the timer
    tracker.handleData("s1", "some output");

    // Advance another 800ms (1600ms total, but only 800ms since last output)
    vi.advanceTimersByTime(800);
    expect(onTimeout).not.toHaveBeenCalled();

    // Advance to trigger timeout (200ms more = 1000ms since last output)
    vi.advanceTimersByTime(201);
    expect(onTimeout).toHaveBeenCalledWith("s1");
    tracker.dispose();
  });

  it("clears timeout on session exit", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ timeoutMs: 1000, onTimeout });
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));
    tracker.handleExit("s1", 0);

    vi.advanceTimersByTime(2000);

    expect(onTimeout).not.toHaveBeenCalled();
    tracker.dispose();
  });

  it("uses default 30 min timeout when not configured", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ onTimeout });
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));

    // 29 minutes — not yet
    vi.advanceTimersByTime(29 * 60 * 1000);
    expect(onTimeout).not.toHaveBeenCalled();

    // 30 minutes + 1ms — fires
    vi.advanceTimersByTime(60 * 1000 + 1);
    expect(onTimeout).toHaveBeenCalledWith("s1");
    tracker.dispose();
  });

  it("does not fire timeout after dispose", () => {
    const onTimeout = vi.fn();
    const tracker = new AgentTracker({ timeoutMs: 1000, onTimeout });
    tracker.registerSession("s1", makeMeta());

    tracker.handleData("s1", oscState("working"));
    tracker.dispose();

    vi.advanceTimersByTime(2000);

    expect(onTimeout).not.toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// listAgentSessions
// ---------------------------------------------------------------------------

describe("AgentTracker — listAgentSessions", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("returns empty array when no sessions registered", () => {
    const tracker = new AgentTracker();
    expect(tracker.listAgentSessions()).toEqual([]);
    tracker.dispose();
  });

  it("returns all registered sessions with metadata", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta({ taskId: "t1", agentType: "claude" }));
    tracker.registerSession("s2", makeMeta({ taskId: "t2", agentType: "codex" }));

    const sessions = tracker.listAgentSessions();
    expect(sessions).toHaveLength(2);

    const s1 = sessions.find((s) => s.sessionId === "s1")!;
    expect(s1.taskId).toBe("t1");
    expect(s1.agentType).toBe("claude");
    expect(s1.state).toBe("idle");

    const s2 = sessions.find((s) => s.sessionId === "s2")!;
    expect(s2.taskId).toBe("t2");
    expect(s2.agentType).toBe("codex");
    tracker.dispose();
  });

  it("includes exited sessions with exit code", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    tracker.handleExit("s1", 1);

    const sessions = tracker.listAgentSessions();
    expect(sessions[0]!.state).toBe("exited");
    expect(sessions[0]!.exitCode).toBe(1);
    tracker.dispose();
  });

  it("removeSession removes from listing", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());
    tracker.registerSession("s2", makeMeta());

    tracker.removeSession("s1");

    const sessions = tracker.listAgentSessions();
    expect(sessions).toHaveLength(1);
    expect(sessions[0]!.sessionId).toBe("s2");
    tracker.dispose();
  });
});

// ---------------------------------------------------------------------------
// Integration with DaemonServer wiring
// ---------------------------------------------------------------------------

describe("AgentTracker — DaemonServer wiring", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("onTimeout callback receives the correct session ID", () => {
    const timeouts: string[] = [];
    const tracker = new AgentTracker({
      timeoutMs: 500,
      onTimeout: (sid) => timeouts.push(sid),
    });

    tracker.registerSession("agent-1", makeMeta());
    tracker.registerSession("agent-2", makeMeta());

    tracker.handleData("agent-1", oscState("working"));
    tracker.handleData("agent-2", oscState("idle"));

    vi.advanceTimersByTime(501);

    // Only agent-1 should timeout (it's in working state)
    expect(timeouts).toEqual(["agent-1"]);
    tracker.dispose();
  });

  it("stateChangedAt is updated on each transition", () => {
    const tracker = new AgentTracker();
    tracker.registerSession("s1", makeMeta());

    const t0 = Date.now();
    tracker.handleData("s1", oscState("working"));

    vi.advanceTimersByTime(5000);
    tracker.handleData("s1", oscState("waiting"));

    const sessions = tracker.listAgentSessions();
    expect(sessions[0]!.stateChangedAt).toBe(t0 + 5000);
    tracker.dispose();
  });
});
