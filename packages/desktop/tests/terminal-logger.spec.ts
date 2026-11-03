/**
 * Tests for terminal-logger.ts — disk logging coordinator.
 *
 * Covers ANSI stripping, log rotation logic, retention policy,
 * and the SessionLogger class behavior.
 */

import { describe, it, expect } from "vitest";
import {
  stripAnsi,
  SessionLogger,
  getExpiredLogFiles,
  isRotatedLogFile,
  DEFAULT_LOGGER_CONFIG,
} from "../src/lib/terminal-logger";

describe("stripAnsi", () => {
  it("removes CSI color sequences", () => {
    const input = "\x1b[31mhello\x1b[0m world";
    expect(stripAnsi(input)).toBe("hello world");
  });

  it("removes SGR sequences with multiple parameters", () => {
    const input = "\x1b[1;32;40mbold green\x1b[0m";
    expect(stripAnsi(input)).toBe("bold green");
  });

  it("removes cursor movement sequences", () => {
    const input = "\x1b[2Jhello\x1b[H\x1b[1;1Hworld";
    expect(stripAnsi(input)).toBe("helloworld");
  });

  it("removes OSC sequences (terminated by BEL)", () => {
    const input = "\x1b]0;window title\x07some text";
    expect(stripAnsi(input)).toBe("some text");
  });

  it("preserves plain text without escape sequences", () => {
    const input = "hello world\nline two\ttab";
    expect(stripAnsi(input)).toBe("hello world\nline two\ttab");
  });

  it("handles empty string", () => {
    expect(stripAnsi("")).toBe("");
  });

  it("handles string with only escape sequences", () => {
    const input = "\x1b[31m\x1b[0m\x1b[2J";
    expect(stripAnsi(input)).toBe("");
  });

  it("preserves unicode characters", () => {
    const input = "\x1b[32m✓\x1b[0m passed — 日本語テスト";
    expect(stripAnsi(input)).toBe("✓ passed — 日本語テスト");
  });

  it("removes multiple consecutive escape sequences", () => {
    const input = "\x1b[1m\x1b[4m\x1b[36munderlined bold cyan\x1b[0m";
    expect(stripAnsi(input)).toBe("underlined bold cyan");
  });
});

describe("SessionLogger", () => {
  it("creates with default config", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs");
    expect(logger.getLogPath()).toBe("/tmp/logs/test-session.log");
    expect(logger.isDisabled()).toBe(false);
  });

  it("respects disabled config", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs", { enabled: false });
    expect(logger.isDisabled()).toBe(true);
  });

  it("buffers writes and returns content on flush", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs");
    logger.write("hello ");
    logger.write("world");
    const content = logger.flush();
    expect(content).toBe("hello world");
  });

  it("strips ANSI when format is plain", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs", { format: "plain" });
    logger.write("\x1b[31mred text\x1b[0m");
    const content = logger.flush();
    expect(content).toBe("red text");
  });

  it("preserves ANSI when format is ansi", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs", { format: "ansi" });
    logger.write("\x1b[31mred text\x1b[0m");
    const content = logger.flush();
    expect(content).toBe("\x1b[31mred text\x1b[0m");
  });

  it("returns null on flush when disabled", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs");
    logger.disable();
    logger.write("data");
    expect(logger.flush()).toBeNull();
  });

  it("returns null on flush when buffer is empty", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs");
    expect(logger.flush()).toBeNull();
  });

  it("provides rotation info when size exceeds threshold", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs", {
      maxSizeBytes: 100,
    });
    logger.setCurrentSize(150);
    const info = logger.getRotationInfo();
    expect(info).not.toBeNull();
    expect(info!.currentPath).toBe("/tmp/logs/test-session.log");
    expect(info!.rotatedPath).toMatch(/test-session\.\d{4}-\d{2}-\d{2}T/);
  });

  it("returns null rotation info when under threshold", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs", {
      maxSizeBytes: 1000,
    });
    logger.setCurrentSize(50);
    expect(logger.getRotationInfo()).toBeNull();
  });

  it("dispose flushes remaining buffer", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs");
    logger.write("final data");
    const content = logger.dispose();
    expect(content).toBe("final data");
  });

  it("disable clears buffer and stops accepting writes", () => {
    const logger = new SessionLogger("test-session", "/tmp/logs");
    logger.write("before");
    logger.disable();
    logger.write("after");
    expect(logger.flush()).toBeNull();
    expect(logger.isDisabled()).toBe(true);
  });
});

describe("getExpiredLogFiles", () => {
  it("returns files older than retention period", () => {
    const now = Date.now();
    const eightDaysAgo = now - 8 * 24 * 60 * 60 * 1000;
    const twoDaysAgo = now - 2 * 24 * 60 * 60 * 1000;

    const files = [
      { name: "old-session.log", mtimeMs: eightDaysAgo },
      { name: "recent-session.log", mtimeMs: twoDaysAgo },
    ];

    const expired = getExpiredLogFiles(files, 7, now);
    expect(expired).toEqual(["old-session.log"]);
  });

  it("returns empty array when no files are expired", () => {
    const now = Date.now();
    const files = [
      { name: "session-1.log", mtimeMs: now - 1000 },
      { name: "session-2.log", mtimeMs: now - 2 * 24 * 60 * 60 * 1000 },
    ];

    const expired = getExpiredLogFiles(files, 7, now);
    expect(expired).toEqual([]);
  });

  it("returns all files when all are expired", () => {
    const now = Date.now();
    const tenDaysAgo = now - 10 * 24 * 60 * 60 * 1000;

    const files = [
      { name: "a.log", mtimeMs: tenDaysAgo },
      { name: "b.log", mtimeMs: tenDaysAgo - 1000 },
    ];

    const expired = getExpiredLogFiles(files, 7, now);
    expect(expired).toHaveLength(2);
  });

  it("handles empty file list", () => {
    expect(getExpiredLogFiles([], 7)).toEqual([]);
  });
});

describe("isRotatedLogFile", () => {
  it("matches rotated log filenames", () => {
    expect(isRotatedLogFile("term-1.2024-01-15T10-30-45-123Z.log")).toBe(true);
    expect(isRotatedLogFile("session-abc.2025-12-31T23-59-59-999Z.log")).toBe(true);
  });

  it("does not match regular log filenames", () => {
    expect(isRotatedLogFile("term-1.log")).toBe(false);
    expect(isRotatedLogFile("session.txt")).toBe(false);
    expect(isRotatedLogFile("2024-01-15.log")).toBe(false);
  });
});
