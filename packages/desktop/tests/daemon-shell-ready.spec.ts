import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  ShellReadyScanner,
  PostReadyFlushGate,
  SHELL_READY_MARKER,
  SHELLS_WITH_READY_MARKER,
  getBashRcfileContent,
  getZshRcfileContent,
} from "../electron/daemon/shell-ready";

// ---------------------------------------------------------------------------
// ShellReadyScanner
// ---------------------------------------------------------------------------

describe("ShellReadyScanner", () => {
  it("detects the marker in a single chunk and strips it from output", () => {
    const scanner = new ShellReadyScanner();
    const input = `some output${SHELL_READY_MARKER}prompt$ `;
    const result = scanner.scan(input);

    expect(result.matched).toBe(true);
    expect(result.output).toBe("some outputprompt$ ");
  });

  it("detects the marker split across two chunks (partial-match buffering)", () => {
    const scanner = new ShellReadyScanner();
    const marker = SHELL_READY_MARKER;
    const splitPoint = Math.floor(marker.length / 2);

    const chunk1 = "prefix" + marker.slice(0, splitPoint);
    const chunk2 = marker.slice(splitPoint) + "suffix";

    const result1 = scanner.scan(chunk1);
    expect(result1.matched).toBe(false);
    // Prefix should be in output, partial marker held back
    expect(result1.output).toBe("prefix");

    const result2 = scanner.scan(chunk2);
    expect(result2.matched).toBe(true);
    expect(result2.output).toBe("suffix");
  });

  it("detects the marker split across three chunks", () => {
    const scanner = new ShellReadyScanner();
    const marker = SHELL_READY_MARKER;
    const split1 = 3;
    const split2 = marker.length - 2;

    const chunk1 = marker.slice(0, split1);
    const chunk2 = marker.slice(split1, split2);
    const chunk3 = marker.slice(split2) + "after";

    const result1 = scanner.scan(chunk1);
    expect(result1.matched).toBe(false);

    const result2 = scanner.scan(chunk2);
    expect(result2.matched).toBe(false);

    const result3 = scanner.scan(chunk3);
    expect(result3.matched).toBe(true);
    expect(result3.output).toBe("after");
  });

  it("flushes held bytes on mismatch", () => {
    const scanner = new ShellReadyScanner();
    // Start of marker followed by a wrong character
    const input = "\x1b]777;crab-shell-readX rest";
    const result = scanner.scan(input);

    expect(result.matched).toBe(false);
    expect(result.output).toBe("\x1b]777;crab-shell-readX rest");
  });

  it("handles data with no marker at all", () => {
    const scanner = new ShellReadyScanner();
    const input = "hello world\r\nprompt$ ";
    const result = scanner.scan(input);

    expect(result.matched).toBe(false);
    expect(result.output).toBe(input);
  });

  it("handles empty input", () => {
    const scanner = new ShellReadyScanner();
    const result = scanner.scan("");

    expect(result.matched).toBe(false);
    expect(result.output).toBe("");
  });

  it("handles marker at the very start of input", () => {
    const scanner = new ShellReadyScanner();
    const input = SHELL_READY_MARKER + "prompt$ ";
    const result = scanner.scan(input);

    expect(result.matched).toBe(true);
    expect(result.output).toBe("prompt$ ");
  });

  it("handles marker at the very end of input", () => {
    const scanner = new ShellReadyScanner();
    const input = "output" + SHELL_READY_MARKER;
    const result = scanner.scan(input);

    expect(result.matched).toBe(true);
    expect(result.output).toBe("output");
  });

  it("handles a false start followed by the real marker", () => {
    const scanner = new ShellReadyScanner();
    // ESC starts a potential match, then fails, then the real marker appears
    const input = "\x1b[32m" + SHELL_READY_MARKER + "done";
    const result = scanner.scan(input);

    expect(result.matched).toBe(true);
    expect(result.output).toBe("\x1b[32mdone");
  });

  it("reset clears partial match state", () => {
    const scanner = new ShellReadyScanner();
    // Feed partial marker
    scanner.scan(SHELL_READY_MARKER.slice(0, 5));
    expect(scanner.getHeldChars().length).toBeGreaterThan(0);

    scanner.reset();
    expect(scanner.getHeldChars()).toBe("");

    // After reset, a full marker in a new chunk should still be detected
    const result = scanner.scan(SHELL_READY_MARKER + "after");
    expect(result.matched).toBe(true);
    expect(result.output).toBe("after");
  });
});

// ---------------------------------------------------------------------------
// PostReadyFlushGate
// ---------------------------------------------------------------------------

describe("PostReadyFlushGate", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("does not flush before arm is called", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.notifyData();
    vi.advanceTimersByTime(100);

    expect(flush).not.toHaveBeenCalled();
  });

  it("defers flush until after prompt draw (data + 30ms delay)", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();
    expect(gate.isPending).toBe(true);

    // Simulate prompt data arriving 10ms after arm
    vi.advanceTimersByTime(10);
    gate.notifyData();
    expect(flush).not.toHaveBeenCalled();

    // 29ms after data — not yet
    vi.advanceTimersByTime(29);
    expect(flush).not.toHaveBeenCalled();

    // 30ms after data — flush fires
    vi.advanceTimersByTime(1);
    expect(flush).toHaveBeenCalledTimes(1);
    expect(gate.isPending).toBe(false);
  });

  it("fires fallback timeout (50ms) if no data arrives after arm", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();

    // 49ms — not yet
    vi.advanceTimersByTime(49);
    expect(flush).not.toHaveBeenCalled();

    // 50ms — fallback fires
    vi.advanceTimersByTime(1);
    expect(flush).toHaveBeenCalledTimes(1);
  });

  it("cancels fallback timer when data arrives", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();

    // Data arrives at 20ms (before 50ms fallback)
    vi.advanceTimersByTime(20);
    gate.notifyData();

    // Advance past the original 50ms fallback
    vi.advanceTimersByTime(30);
    // Should have flushed exactly once (from data timer, not fallback)
    expect(flush).toHaveBeenCalledTimes(1);
  });

  it("uses the latest data chunk for the 30ms delay", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();

    // First data chunk at 10ms
    vi.advanceTimersByTime(10);
    gate.notifyData();

    // Second data chunk at 25ms (resets the 30ms timer)
    vi.advanceTimersByTime(15);
    gate.notifyData();

    // 29ms after second data — not yet
    vi.advanceTimersByTime(29);
    expect(flush).not.toHaveBeenCalled();

    // 30ms after second data — flush fires
    vi.advanceTimersByTime(1);
    expect(flush).toHaveBeenCalledTimes(1);
  });

  it("clear cancels all pending timers", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();
    vi.advanceTimersByTime(10);
    gate.notifyData();

    // Clear before the 30ms data timer fires
    gate.clear();

    vi.advanceTimersByTime(100);
    expect(flush).not.toHaveBeenCalled();
    expect(gate.isPending).toBe(false);
  });

  it("clear cancels fallback timer", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();
    gate.clear();

    vi.advanceTimersByTime(100);
    expect(flush).not.toHaveBeenCalled();
  });

  it("flush only fires once even with multiple data notifications", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();
    gate.notifyData();
    vi.advanceTimersByTime(30);
    expect(flush).toHaveBeenCalledTimes(1);

    // Additional data after flush should not trigger again
    gate.notifyData();
    vi.advanceTimersByTime(100);
    expect(flush).toHaveBeenCalledTimes(1);
  });

  it("arm is idempotent", () => {
    const flush = vi.fn();
    const gate = new PostReadyFlushGate(flush);

    gate.arm();
    gate.arm(); // second arm should be ignored

    vi.advanceTimersByTime(50);
    // Only one fallback timer should have fired
    expect(flush).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// Shell rcfile content
// ---------------------------------------------------------------------------

describe("shell rcfile wrappers", () => {
  it("bash rcfile emits the shell-ready marker", () => {
    const content = getBashRcfileContent();
    expect(content).toContain("777;crab-shell-ready");
    expect(content).toContain("\\033]777;crab-shell-ready\\007");
  });

  it("bash rcfile sources login files in correct priority order", () => {
    const content = getBashRcfileContent();
    expect(content).toContain('source "$HOME/.bash_profile"');
    expect(content).toContain('source "$HOME/.bash_login"');
    expect(content).toContain('source "$HOME/.profile"');
  });

  it("bash rcfile uses PROMPT_COMMAND to emit marker", () => {
    const content = getBashRcfileContent();
    expect(content).toContain("PROMPT_COMMAND");
    expect(content).toContain("__crab_prompt_mark");
  });

  it("bash rcfile handles both array and scalar PROMPT_COMMAND", () => {
    const content = getBashRcfileContent();
    // Array form (Bash 5.1+)
    expect(content).toContain('declare -p PROMPT_COMMAND');
    expect(content).toContain('"declare -a"');
  });

  it("zsh rcfile emits the shell-ready marker", () => {
    const content = getZshRcfileContent();
    expect(content).toContain("777;crab-shell-ready");
    expect(content).toContain("\\033]777;crab-shell-ready\\007");
  });

  it("zsh rcfile sources user .zshrc", () => {
    const content = getZshRcfileContent();
    expect(content).toContain('source "$_crab_home/.zshrc"');
  });

  it("zsh rcfile uses precmd_functions to emit marker", () => {
    const content = getZshRcfileContent();
    expect(content).toContain("precmd_functions");
    expect(content).toContain("__crab_prompt_mark");
  });

  it("SHELLS_WITH_READY_MARKER includes zsh and bash", () => {
    expect(SHELLS_WITH_READY_MARKER.has("zsh")).toBe(true);
    expect(SHELLS_WITH_READY_MARKER.has("bash")).toBe(true);
    expect(SHELLS_WITH_READY_MARKER.has("sh")).toBe(false);
  });
});
