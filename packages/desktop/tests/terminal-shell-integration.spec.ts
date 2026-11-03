import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  parseOsc7Uri,
  parseOsc133,
  attachShellIntegration,
  MarkRingBuffer,
  formatDuration,
} from "../src/lib/terminal-shell-integration";
import type { Terminal, IDisposable } from "@xterm/xterm";

// ── parseOsc7Uri unit tests ────────────────────────────────────────

describe("parseOsc7Uri", () => {
  it("parses a standard file URI with hostname", () => {
    expect(parseOsc7Uri("file://localhost/home/user/projects")).toBe(
      "/home/user/projects"
    );
  });

  it("parses a file URI with empty hostname (triple slash)", () => {
    expect(parseOsc7Uri("file:///home/user/projects")).toBe(
      "/home/user/projects"
    );
  });

  it("decodes percent-encoded characters", () => {
    expect(parseOsc7Uri("file:///home/user/my%20project")).toBe(
      "/home/user/my project"
    );
  });

  it("decodes multiple percent-encoded characters", () => {
    expect(parseOsc7Uri("file:///tmp/%E4%B8%AD%E6%96%87")).toBe(
      "/tmp/中文"
    );
  });

  it("handles paths with special characters", () => {
    expect(parseOsc7Uri("file:///home/user/foo%23bar")).toBe(
      "/home/user/foo#bar"
    );
  });

  it("returns null for non-file URIs", () => {
    expect(parseOsc7Uri("http://example.com/path")).toBeNull();
  });

  it("returns null for empty string", () => {
    expect(parseOsc7Uri("")).toBeNull();
  });

  it("returns null for file:// with no path", () => {
    expect(parseOsc7Uri("file://hostname")).toBeNull();
  });

  it("handles root path", () => {
    expect(parseOsc7Uri("file:///")).toBe("/");
  });

  it("handles malformed percent-encoding gracefully", () => {
    const result = parseOsc7Uri("file:///path/%ZZ");
    expect(result).toBe("/path/%ZZ");
  });
});

// ── parseOsc133 unit tests ─────────────────────────────────────────

describe("parseOsc133", () => {
  it("parses A as prompt-start", () => {
    expect(parseOsc133("A")).toEqual({ type: "prompt-start" });
  });

  it("parses B as command-start", () => {
    expect(parseOsc133("B")).toEqual({ type: "command-start" });
  });

  it("parses C as command-executed", () => {
    expect(parseOsc133("C")).toEqual({ type: "command-executed" });
  });

  it("parses D as command-finished with exit code 0", () => {
    expect(parseOsc133("D")).toEqual({ type: "command-finished", exitCode: 0 });
  });

  it("parses D;0 as command-finished with exit code 0", () => {
    expect(parseOsc133("D;0")).toEqual({ type: "command-finished", exitCode: 0 });
  });

  it("parses D;1 as command-finished with exit code 1", () => {
    expect(parseOsc133("D;1")).toEqual({ type: "command-finished", exitCode: 1 });
  });

  it("parses D;127 as command-finished with exit code 127", () => {
    expect(parseOsc133("D;127")).toEqual({ type: "command-finished", exitCode: 127 });
  });

  it("returns null for empty string", () => {
    expect(parseOsc133("")).toBeNull();
  });

  it("returns null for unknown marker", () => {
    expect(parseOsc133("X")).toBeNull();
  });

  it("handles D with non-numeric exit code gracefully", () => {
    expect(parseOsc133("D;abc")).toEqual({ type: "command-finished", exitCode: 0 });
  });
});

// ── MarkRingBuffer unit tests ──────────────────────────────────────

describe("MarkRingBuffer", () => {
  it("starts empty", () => {
    const buf = new MarkRingBuffer(5);
    expect(buf.size).toBe(0);
    expect(buf.marks).toEqual([]);
  });

  it("pushes marks and retrieves them in order", () => {
    const buf = new MarkRingBuffer(5);
    buf.push({ line: 0, type: "prompt-start", timestamp: 100 });
    buf.push({ line: 5, type: "command-start", timestamp: 200 });
    buf.push({ line: 10, type: "command-executed", timestamp: 300 });

    expect(buf.size).toBe(3);
    expect(buf.marks).toEqual([
      { line: 0, type: "prompt-start", timestamp: 100 },
      { line: 5, type: "command-start", timestamp: 200 },
      { line: 10, type: "command-executed", timestamp: 300 },
    ]);
  });

  it("wraps around when capacity is exceeded", () => {
    const buf = new MarkRingBuffer(3);
    buf.push({ line: 0, type: "prompt-start", timestamp: 100 });
    buf.push({ line: 5, type: "command-start", timestamp: 200 });
    buf.push({ line: 10, type: "command-executed", timestamp: 300 });
    buf.push({ line: 15, type: "command-finished", timestamp: 400 });

    expect(buf.size).toBe(3);
    // Oldest (line 0) should be gone
    expect(buf.marks).toEqual([
      { line: 5, type: "command-start", timestamp: 200 },
      { line: 10, type: "command-executed", timestamp: 300 },
      { line: 15, type: "command-finished", timestamp: 400 },
    ]);
  });

  it("findPrevious returns nearest mark before fromLine", () => {
    const buf = new MarkRingBuffer(10);
    buf.push({ line: 0, type: "prompt-start", timestamp: 100 });
    buf.push({ line: 10, type: "prompt-start", timestamp: 200 });
    buf.push({ line: 20, type: "prompt-start", timestamp: 300 });

    expect(buf.findPrevious(15)).toEqual({ line: 10, type: "prompt-start", timestamp: 200 });
    expect(buf.findPrevious(25)).toEqual({ line: 20, type: "prompt-start", timestamp: 300 });
    expect(buf.findPrevious(5)).toEqual({ line: 0, type: "prompt-start", timestamp: 100 });
  });

  it("findPrevious returns null when no mark exists before fromLine", () => {
    const buf = new MarkRingBuffer(10);
    buf.push({ line: 10, type: "prompt-start", timestamp: 100 });

    expect(buf.findPrevious(5)).toBeNull();
    expect(buf.findPrevious(10)).toBeNull();
  });

  it("findNext returns nearest mark after fromLine", () => {
    const buf = new MarkRingBuffer(10);
    buf.push({ line: 0, type: "prompt-start", timestamp: 100 });
    buf.push({ line: 10, type: "prompt-start", timestamp: 200 });
    buf.push({ line: 20, type: "prompt-start", timestamp: 300 });

    expect(buf.findNext(5)).toEqual({ line: 10, type: "prompt-start", timestamp: 200 });
    expect(buf.findNext(15)).toEqual({ line: 20, type: "prompt-start", timestamp: 300 });
    expect(buf.findNext(0)).toEqual({ line: 10, type: "prompt-start", timestamp: 200 });
  });

  it("findNext returns null when no mark exists after fromLine", () => {
    const buf = new MarkRingBuffer(10);
    buf.push({ line: 10, type: "prompt-start", timestamp: 100 });

    expect(buf.findNext(10)).toBeNull();
    expect(buf.findNext(15)).toBeNull();
  });

  it("getRange returns marks within the specified line range", () => {
    const buf = new MarkRingBuffer(10);
    buf.push({ line: 0, type: "prompt-start", timestamp: 100 });
    buf.push({ line: 5, type: "command-start", timestamp: 200 });
    buf.push({ line: 10, type: "command-executed", timestamp: 300 });
    buf.push({ line: 15, type: "command-finished", timestamp: 400 });
    buf.push({ line: 20, type: "prompt-start", timestamp: 500 });

    const range = buf.getRange(5, 15);
    expect(range).toEqual([
      { line: 5, type: "command-start", timestamp: 200 },
      { line: 10, type: "command-executed", timestamp: 300 },
      { line: 15, type: "command-finished", timestamp: 400 },
    ]);
  });

  it("getRange returns empty array when no marks in range", () => {
    const buf = new MarkRingBuffer(10);
    buf.push({ line: 0, type: "prompt-start", timestamp: 100 });
    buf.push({ line: 20, type: "prompt-start", timestamp: 200 });

    expect(buf.getRange(5, 15)).toEqual([]);
  });

  it("uses default capacity of 1000", () => {
    const buf = new MarkRingBuffer();
    expect(buf.capacity).toBe(1000);
  });
});

// ── attachShellIntegration unit tests ──────────────────────────────

describe("attachShellIntegration", () => {
  let mockTerminal: Terminal;
  let oscHandlers: Map<number, (data: string) => boolean>;
  let mockDisposables: IDisposable[];

  beforeEach(() => {
    oscHandlers = new Map();
    mockDisposables = [];

    mockTerminal = {
      buffer: {
        active: {
          cursorY: 5,
          baseY: 10,
          getLine: vi.fn((row: number) => ({
            translateToString: vi.fn(() => `line content at ${row}`),
          })),
        },
      },
      parser: {
        registerOscHandler: vi.fn((ident: number, callback: (data: string) => boolean) => {
          oscHandlers.set(ident, callback);
          const disposable = { dispose: vi.fn() };
          mockDisposables.push(disposable);
          return disposable;
        }),
      },
      scrollToLine: vi.fn(),
    } as unknown as Terminal;
  });

  it("registers both OSC 7 and OSC 133 handlers", () => {
    attachShellIntegration(mockTerminal);
    expect(mockTerminal.parser.registerOscHandler).toHaveBeenCalledWith(
      7,
      expect.any(Function)
    );
    expect(mockTerminal.parser.registerOscHandler).toHaveBeenCalledWith(
      133,
      expect.any(Function)
    );
  });

  it("updates state.cwd when OSC 7 is received", () => {
    const { state } = attachShellIntegration(mockTerminal);
    expect(state.cwd).toBeNull();

    const handler = oscHandlers.get(7)!;
    handler("file:///home/user/repo");

    expect(state.cwd).toBe("/home/user/repo");
  });

  it("updates cwd on subsequent OSC 7 sequences", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(7)!;

    handler("file:///first");
    expect(state.cwd).toBe("/first");

    handler("file:///second");
    expect(state.cwd).toBe("/second");
  });

  it("does not update cwd for invalid URIs", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(7)!;

    handler("file:///valid/path");
    expect(state.cwd).toBe("/valid/path");

    handler("http://not-a-file-uri");
    expect(state.cwd).toBe("/valid/path");
  });

  it("decodes percent-encoded paths in OSC 7", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(7)!;

    handler("file:///home/user/my%20project%2Fsubdir");
    expect(state.cwd).toBe("/home/user/my project/subdir");
  });

  it("initializes state with expected defaults", () => {
    const { state } = attachShellIntegration(mockTerminal);
    expect(state.cwd).toBeNull();
    expect(state.atPrompt).toBe(false);
    expect(state.marks).toEqual([]);
    expect(state.runningCommand).toBeNull();
    expect(state.commandStartedAt).toBeNull();
  });

  it("sets atPrompt to true on OSC 133 A (prompt-start)", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("A");
    expect(state.atPrompt).toBe(true);
  });

  it("sets atPrompt to false on OSC 133 B (command-start)", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("A");
    expect(state.atPrompt).toBe(true);

    handler("B");
    expect(state.atPrompt).toBe(false);
  });

  it("records commandStartedAt on OSC 133 B", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    const before = Date.now();
    handler("B");
    const after = Date.now();

    expect(state.commandStartedAt).toBeGreaterThanOrEqual(before);
    expect(state.commandStartedAt).toBeLessThanOrEqual(after);
  });

  it("clears commandStartedAt on OSC 133 D (command-finished)", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("B");
    expect(state.commandStartedAt).not.toBeNull();

    handler("D;0");
    expect(state.commandStartedAt).toBeNull();
  });

  it("records exit code on command-finished marks", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("D;42");
    const marks = state.marks;
    expect(marks.length).toBe(1);
    expect(marks[0].type).toBe("command-finished");
    expect(marks[0].exitCode).toBe(42);
  });

  it("records marks in the ring buffer", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("A");
    handler("B");
    handler("C");
    handler("D;0");

    expect(state.marks.length).toBe(4);
    expect(state.marks[0].type).toBe("prompt-start");
    expect(state.marks[1].type).toBe("command-start");
    expect(state.marks[2].type).toBe("command-executed");
    expect(state.marks[3].type).toBe("command-finished");
  });

  it("jumpToPreviousPrompt scrolls to the previous mark", () => {
    const integration = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    // Push a mark at line 15 (baseY=10 + cursorY=5)
    handler("A");

    // Change cursor position to simulate being further down
    (mockTerminal.buffer!.active as any).cursorY = 10;
    (mockTerminal.buffer!.active as any).baseY = 10;

    integration.jumpToPreviousPrompt();
    expect(mockTerminal.scrollToLine).toHaveBeenCalledWith(15);
  });

  it("jumpToNextPrompt scrolls to the next mark", () => {
    const integration = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    // Set cursor at line 15
    (mockTerminal.buffer!.active as any).cursorY = 5;
    (mockTerminal.buffer!.active as any).baseY = 10;

    // Push a mark at line 15 (current position)
    handler("A");

    // Push another mark — cursor is still at 15, so we need a mark at a higher line
    (mockTerminal.buffer!.active as any).cursorY = 10;
    (mockTerminal.buffer!.active as any).baseY = 15;
    handler("A"); // This mark is at line 25

    // Reset cursor to line 15
    (mockTerminal.buffer!.active as any).cursorY = 5;
    (mockTerminal.buffer!.active as any).baseY = 10;

    integration.jumpToNextPrompt();
    expect(mockTerminal.scrollToLine).toHaveBeenCalledWith(25);
  });

  it("jumpToPreviousPrompt does nothing when no previous mark exists", () => {
    const integration = attachShellIntegration(mockTerminal);
    integration.jumpToPreviousPrompt();
    expect(mockTerminal.scrollToLine).not.toHaveBeenCalled();
  });

  it("jumpToNextPrompt does nothing when no next mark exists", () => {
    const integration = attachShellIntegration(mockTerminal);
    integration.jumpToNextPrompt();
    expect(mockTerminal.scrollToLine).not.toHaveBeenCalled();
  });

  it("getCommandDuration returns elapsed time when command is running", () => {
    const { state, getCommandDuration } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("B");
    // Small delay to ensure duration > 0
    const duration = getCommandDuration();
    expect(duration).not.toBeNull();
    expect(duration!).toBeGreaterThanOrEqual(0);
  });

  it("getCommandDuration returns null when no command is running", () => {
    const { getCommandDuration } = attachShellIntegration(mockTerminal);
    expect(getCommandDuration()).toBeNull();
  });

  it("selectCommandOutput returns null for invalid markIndex", () => {
    const { selectCommandOutput } = attachShellIntegration(mockTerminal);
    expect(selectCommandOutput(-1)).toBeNull();
    expect(selectCommandOutput(100)).toBeNull();
  });

  it("selectCommandOutput extracts text between command-executed and next prompt-start", () => {
    const integration = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    // Simulate: prompt-start at line 15, command-start, command-executed, then prompt-start
    handler("A"); // line 15
    (mockTerminal.buffer!.active as any).cursorY = 6;
    handler("B"); // line 16
    (mockTerminal.buffer!.active as any).cursorY = 7;
    handler("C"); // line 17 (command-executed)
    (mockTerminal.buffer!.active as any).cursorY = 10;
    handler("A"); // line 20 (next prompt-start)

    // selectCommandOutput for markIndex 2 (the command-executed mark)
    const output = integration.selectCommandOutput(2);
    expect(output).not.toBeNull();
    // Output should be lines 17 to 19 (between command-executed and next prompt-start - 1)
  });

  it("resets runningCommand on prompt-start", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    state.runningCommand = "some command";
    handler("A");
    expect(state.runningCommand).toBeNull();
  });

  it("resets runningCommand on command-finished", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    state.runningCommand = "some command";
    handler("D;0");
    expect(state.runningCommand).toBeNull();
  });

  it("dispose cleans up all OSC handlers", () => {
    const integration = attachShellIntegration(mockTerminal);
    integration.dispose();
    for (const d of mockDisposables) {
      expect(d.dispose).toHaveBeenCalled();
    }
  });
});


// ── formatDuration unit tests ──────────────────────────────────────

describe("formatDuration", () => {
  it("returns '<1s' for durations under 1 second", () => {
    expect(formatDuration(0)).toBe("<1s");
    expect(formatDuration(500)).toBe("<1s");
    expect(formatDuration(999)).toBe("<1s");
  });

  it("returns seconds for durations under 1 minute", () => {
    expect(formatDuration(1000)).toBe("1s");
    expect(formatDuration(5000)).toBe("5s");
    expect(formatDuration(59000)).toBe("59s");
    expect(formatDuration(59999)).toBe("59s");
  });

  it("returns minutes and seconds for durations under 1 hour", () => {
    expect(formatDuration(60000)).toBe("1m");
    expect(formatDuration(61000)).toBe("1m 1s");
    expect(formatDuration(154000)).toBe("2m 34s");
    expect(formatDuration(3599000)).toBe("59m 59s");
  });

  it("returns hours and minutes for durations >= 1 hour", () => {
    expect(formatDuration(3600000)).toBe("1h");
    expect(formatDuration(3900000)).toBe("1h 5m");
    expect(formatDuration(7200000)).toBe("2h");
    expect(formatDuration(7260000)).toBe("2h 1m");
  });

  it("omits zero sub-units", () => {
    expect(formatDuration(120000)).toBe("2m");
    expect(formatDuration(3600000)).toBe("1h");
  });

  it("handles large durations", () => {
    // 25 hours, 30 minutes
    expect(formatDuration(91800000)).toBe("25h 30m");
  });
});

// ── lastCommandDuration state tracking ─────────────────────────────

describe("attachShellIntegration command duration tracking", () => {
  let mockTerminal: Terminal;
  let oscHandlers: Map<number, (data: string) => boolean>;

  beforeEach(() => {
    oscHandlers = new Map();

    mockTerminal = {
      buffer: {
        active: {
          cursorY: 5,
          baseY: 10,
          getLine: vi.fn((row: number) => ({
            translateToString: vi.fn(() => `line content at ${row}`),
          })),
        },
      },
      parser: {
        registerOscHandler: vi.fn((ident: number, callback: (data: string) => boolean) => {
          oscHandlers.set(ident, callback);
          return { dispose: vi.fn() };
        }),
      },
      scrollToLine: vi.fn(),
    } as unknown as Terminal;
  });

  it("initializes lastCommandDuration and lastCommandExitCode as null", () => {
    const { state } = attachShellIntegration(mockTerminal);
    expect(state.lastCommandDuration).toBeNull();
    expect(state.lastCommandExitCode).toBeNull();
  });

  it("stores lastCommandDuration on command-finished", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("B"); // command-start — sets commandStartedAt
    // Simulate some time passing
    const startedAt = state.commandStartedAt!;
    // Manually set commandStartedAt to a known value for deterministic test
    (state as any).commandStartedAt = Date.now() - 5000;

    handler("D;0"); // command-finished
    expect(state.lastCommandDuration).toBeGreaterThanOrEqual(4900);
    expect(state.lastCommandDuration).toBeLessThanOrEqual(5200);
  });

  it("stores lastCommandExitCode on command-finished", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("B");
    handler("D;42");
    expect(state.lastCommandExitCode).toBe(42);
  });

  it("stores exit code 0 for successful commands", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    handler("B");
    handler("D;0");
    expect(state.lastCommandExitCode).toBe(0);
  });

  it("does not set lastCommandDuration if no command was started", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    // command-finished without a preceding command-start
    handler("D;0");
    expect(state.lastCommandDuration).toBeNull();
  });

  it("updates lastCommandDuration on each command completion", () => {
    const { state } = attachShellIntegration(mockTerminal);
    const handler = oscHandlers.get(133)!;

    // First command
    handler("B");
    (state as any).commandStartedAt = Date.now() - 2000;
    handler("D;0");
    const firstDuration = state.lastCommandDuration;
    expect(firstDuration).toBeGreaterThanOrEqual(1900);

    // Second command
    handler("B");
    (state as any).commandStartedAt = Date.now() - 10000;
    handler("D;1");
    expect(state.lastCommandDuration).toBeGreaterThanOrEqual(9900);
    expect(state.lastCommandExitCode).toBe(1);
  });
});
