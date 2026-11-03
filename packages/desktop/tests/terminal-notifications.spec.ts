import { describe, it, expect } from "vitest";
import {
  shouldNotify,
  formatNotification,
  DEFAULT_NOTIFICATION_CONFIG,
  type NotificationConfig,
  type CommandCompletion,
} from "../src/lib/terminal-notifications";

describe("terminal-notifications", () => {
  describe("shouldNotify", () => {
    const baseCompletion: CommandCompletion = {
      command: "npm run build",
      exitCode: 0,
      durationMs: 20000,
      terminalId: "term-1",
      tabFocused: false,
    };

    it("returns true when all conditions are met (enabled, duration > threshold, tab not focused)", () => {
      expect(shouldNotify(baseCompletion, DEFAULT_NOTIFICATION_CONFIG)).toBe(true);
    });

    it("returns false when notifications are disabled", () => {
      const config: NotificationConfig = { ...DEFAULT_NOTIFICATION_CONFIG, enabled: false };
      expect(shouldNotify(baseCompletion, config)).toBe(false);
    });

    it("returns false when duration is below threshold", () => {
      const completion: CommandCompletion = { ...baseCompletion, durationMs: 5000 };
      expect(shouldNotify(completion, DEFAULT_NOTIFICATION_CONFIG)).toBe(false);
    });

    it("returns false when duration equals threshold (not exceeding)", () => {
      const completion: CommandCompletion = { ...baseCompletion, durationMs: 15000 };
      expect(shouldNotify(completion, DEFAULT_NOTIFICATION_CONFIG)).toBe(false);
    });

    it("returns false when tab is focused", () => {
      const completion: CommandCompletion = { ...baseCompletion, tabFocused: true };
      expect(shouldNotify(completion, DEFAULT_NOTIFICATION_CONFIG)).toBe(false);
    });

    it("returns true with custom threshold when duration exceeds it", () => {
      const config: NotificationConfig = { ...DEFAULT_NOTIFICATION_CONFIG, durationThresholdMs: 5000 };
      const completion: CommandCompletion = { ...baseCompletion, durationMs: 6000 };
      expect(shouldNotify(completion, config)).toBe(true);
    });

    it("returns false when multiple conditions fail simultaneously", () => {
      const config: NotificationConfig = { ...DEFAULT_NOTIFICATION_CONFIG, enabled: false };
      const completion: CommandCompletion = { ...baseCompletion, tabFocused: true, durationMs: 1000 };
      expect(shouldNotify(completion, config)).toBe(false);
    });
  });

  describe("formatNotification", () => {
    it("returns success icon for exit code 0", () => {
      const completion: CommandCompletion = {
        command: "npm test",
        exitCode: 0,
        durationMs: 30000,
        terminalId: "term-1",
        tabFocused: false,
      };
      const result = formatNotification(completion);
      expect(result.icon).toBe("success");
      expect(result.title).toBe("Command completed");
      expect(result.body).toContain("npm test");
      expect(result.body).toContain("30s");
    });

    it("returns error icon for non-zero exit code", () => {
      const completion: CommandCompletion = {
        command: "cargo build",
        exitCode: 1,
        durationMs: 45000,
        terminalId: "term-2",
        tabFocused: false,
      };
      const result = formatNotification(completion);
      expect(result.icon).toBe("error");
      expect(result.title).toBe("Command failed (exit 1)");
      expect(result.body).toContain("cargo build");
    });

    it("truncates long commands to 80 characters", () => {
      const longCommand = "a".repeat(100);
      const completion: CommandCompletion = {
        command: longCommand,
        exitCode: 0,
        durationMs: 20000,
        terminalId: "term-1",
        tabFocused: false,
      };
      const result = formatNotification(completion);
      // Body should contain truncated command (77 chars + "...")
      expect(result.body).toContain("a".repeat(77) + "...");
      expect(result.body).not.toContain("a".repeat(78));
    });

    it("formats duration in minutes and seconds for longer commands", () => {
      const completion: CommandCompletion = {
        command: "make build",
        exitCode: 0,
        durationMs: 125000, // 2m 5s
        terminalId: "term-1",
        tabFocused: false,
      };
      const result = formatNotification(completion);
      expect(result.body).toContain("2m 5s");
    });

    it("formats duration in hours for very long commands", () => {
      const completion: CommandCompletion = {
        command: "make build",
        exitCode: 0,
        durationMs: 3725000, // 1h 2m
        terminalId: "term-1",
        tabFocused: false,
      };
      const result = formatNotification(completion);
      expect(result.body).toContain("1h 2m");
    });

    it("handles exit code 127 (command not found)", () => {
      const completion: CommandCompletion = {
        command: "nonexistent-tool",
        exitCode: 127,
        durationMs: 16000,
        terminalId: "term-1",
        tabFocused: false,
      };
      const result = formatNotification(completion);
      expect(result.icon).toBe("error");
      expect(result.title).toBe("Command failed (exit 127)");
    });
  });
});
