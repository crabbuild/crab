import { describe, it, expect } from "vitest";
import { formatExitInfo } from "../src/components/TerminalCrashBanner";

describe("terminal-crash-recovery", () => {
  describe("formatExitInfo", () => {
    it("formats signal-based termination", () => {
      const result = formatExitInfo(137, "SIGKILL");
      expect(result).toContain("SIGKILL");
    });

    it("formats exit code 1 as generic error", () => {
      const result = formatExitInfo(1);
      expect(result).toContain("error");
      expect(result).toContain("1");
    });

    it("formats exit code 127 as command not found", () => {
      const result = formatExitInfo(127);
      expect(result).toContain("not found");
    });

    it("formats exit code 130 as Ctrl+C interrupt", () => {
      const result = formatExitInfo(130);
      expect(result).toContain("Ctrl+C");
    });

    it("formats exit code 137 as SIGKILL", () => {
      const result = formatExitInfo(137);
      expect(result).toContain("killed");
      expect(result).toContain("SIGKILL");
    });

    it("formats exit code 139 as segfault", () => {
      const result = formatExitInfo(139);
      expect(result).toContain("Segmentation fault");
    });

    it("formats exit code 143 as SIGTERM", () => {
      const result = formatExitInfo(143);
      expect(result).toContain("terminated");
      expect(result).toContain("SIGTERM");
    });

    it("formats signal-range exit codes (128+N)", () => {
      const result = formatExitInfo(134); // 128 + 6 (SIGABRT)
      expect(result).toContain("signal 6");
    });

    it("formats unknown exit codes with the code number", () => {
      const result = formatExitInfo(42);
      expect(result).toContain("42");
    });

    it("prioritizes signal name over exit code", () => {
      const result = formatExitInfo(9, "SIGSEGV");
      expect(result).toContain("SIGSEGV");
      expect(result).not.toContain("code 9");
    });
  });
});
