import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import {
  readPidFile,
  detectAndHandlePathMismatch,
  type PidFileData,
  type PathMismatchDetectionOptions,
} from "../electron/daemon/path-mismatch-detection";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "path-mismatch-test-"));
}

function writePidFile(pidPath: string, data: PidFileData): void {
  fs.writeFileSync(pidPath, JSON.stringify(data));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("path-mismatch-detection", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = createTmpDir();
  });

  afterEach(() => {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  // -------------------------------------------------------------------------
  // readPidFile
  // -------------------------------------------------------------------------

  describe("readPidFile", () => {
    it("returns parsed PID data when file is valid", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      const data: PidFileData = {
        pid: 12345,
        startedAtMs: Date.now(),
        entryPath: "/app/v1/daemon-main.js",
      };
      writePidFile(pidPath, data);

      const result = readPidFile(pidPath);
      expect(result).toEqual(data);
    });

    it("returns null when file does not exist", () => {
      const pidPath = path.join(tmpDir, "nonexistent.pid");
      const result = readPidFile(pidPath);
      expect(result).toBeNull();
    });

    it("returns null when file contains invalid JSON", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      fs.writeFileSync(pidPath, "not valid json {{{");

      const result = readPidFile(pidPath);
      expect(result).toBeNull();
    });

    it("returns null when required fields are missing", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      fs.writeFileSync(pidPath, JSON.stringify({ pid: 123 }));

      const result = readPidFile(pidPath);
      expect(result).toBeNull();
    });

    it("returns null when fields have wrong types", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      fs.writeFileSync(
        pidPath,
        JSON.stringify({ pid: "not-a-number", startedAtMs: 123, entryPath: "/path" }),
      );

      const result = readPidFile(pidPath);
      expect(result).toBeNull();
    });
  });

  // -------------------------------------------------------------------------
  // detectAndHandlePathMismatch — no mismatch cases
  // -------------------------------------------------------------------------

  describe("detectAndHandlePathMismatch (no mismatch)", () => {
    it("returns no mismatch when PID file does not exist", () => {
      const opts: PathMismatchDetectionOptions = {
        pidPath: path.join(tmpDir, "nonexistent.pid"),
        socketPath: path.join(tmpDir, "daemon-v1.sock"),
        tokenPath: path.join(tmpDir, "daemon-v1.token"),
        currentEntryPath: "/app/v2/daemon-main.js",
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(false);
      expect(result.pidFileEntryPath).toBeNull();
      expect(result.killed).toBe(false);
      expect(result.cleanedUp).toBe(false);
    });

    it("returns no mismatch when paths are identical", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      const entryPath = "/app/v1/daemon-main.js";
      writePidFile(pidPath, { pid: 12345, startedAtMs: Date.now(), entryPath });

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath: path.join(tmpDir, "daemon-v1.sock"),
        tokenPath: path.join(tmpDir, "daemon-v1.token"),
        currentEntryPath: entryPath,
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(false);
      expect(result.pidFileEntryPath).toBe(entryPath);
      expect(result.killed).toBe(false);
      expect(result.cleanedUp).toBe(false);
    });

    it("returns no mismatch when paths differ only by trailing slash", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      writePidFile(pidPath, {
        pid: 12345,
        startedAtMs: Date.now(),
        entryPath: "/app/v1/daemon-main.js/",
      });

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath: path.join(tmpDir, "daemon-v1.sock"),
        tokenPath: path.join(tmpDir, "daemon-v1.token"),
        currentEntryPath: "/app/v1/daemon-main.js",
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(false);
    });

    it("returns no mismatch when PID file has invalid JSON", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      fs.writeFileSync(pidPath, "garbage");

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath: path.join(tmpDir, "daemon-v1.sock"),
        tokenPath: path.join(tmpDir, "daemon-v1.token"),
        currentEntryPath: "/app/v2/daemon-main.js",
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(false);
      expect(result.pidFileEntryPath).toBeNull();
    });
  });

  // -------------------------------------------------------------------------
  // detectAndHandlePathMismatch — mismatch with live process
  // -------------------------------------------------------------------------

  describe("detectAndHandlePathMismatch (mismatch, process alive)", () => {
    it("preserves the live daemon and leaves connection files intact when paths differ", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");

      // Create stale files
      writePidFile(pidPath, {
        pid: 99999,
        startedAtMs: Date.now(),
        entryPath: "/old/app/daemon-main.js",
      });
      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(tokenPath, "old-token");

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath,
        tokenPath,
        currentEntryPath: "/new/app/daemon-main.js",
        isProcessAlive: () => true,
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(true);
      expect(result.pidFileEntryPath).toBe("/old/app/daemon-main.js");
      expect(result.currentEntryPath).toBe("/new/app/daemon-main.js");
      expect(result.killed).toBe(false);
      expect(result.cleanedUp).toBe(false);

      // Verify files were left intact so the app can probe/reuse the daemon.
      expect(fs.existsSync(socketPath)).toBe(true);
      expect(fs.existsSync(tokenPath)).toBe(true);
      expect(fs.existsSync(pidPath)).toBe(true);
    });

    it("logs a warning when live daemon replacement is deferred", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");

      writePidFile(pidPath, {
        pid: 99999,
        startedAtMs: Date.now(),
        entryPath: "/old/path/daemon.js",
      });
      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(tokenPath, "token");

      const logMessages: Array<{ level: string; message: string }> = [];

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath,
        tokenPath,
        currentEntryPath: "/new/path/daemon.js",
        isProcessAlive: () => true,
        log: (level, message) => {
          logMessages.push({ level, message });
        },
      };

      detectAndHandlePathMismatch(opts);

      // Should have logged a warning about the mismatch
      const warnings = logMessages.filter((m) => m.level === "warn");
      expect(warnings.length).toBeGreaterThanOrEqual(1);
      expect(warnings[0]!.message).toContain("mismatch");
    });
  });

  // -------------------------------------------------------------------------
  // detectAndHandlePathMismatch — mismatch with dead process
  // -------------------------------------------------------------------------

  describe("detectAndHandlePathMismatch (mismatch, process dead)", () => {
    it("cleans up files without killing when process is already dead", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");

      writePidFile(pidPath, {
        pid: 99999,
        startedAtMs: Date.now(),
        entryPath: "/old/daemon.js",
      });
      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(tokenPath, "token");

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath,
        tokenPath,
        currentEntryPath: "/new/daemon.js",
        isProcessAlive: () => false,
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(true);
      expect(result.killed).toBe(false);
      expect(result.cleanedUp).toBe(true);

      // Verify files were removed
      expect(fs.existsSync(socketPath)).toBe(false);
      expect(fs.existsSync(tokenPath)).toBe(false);
      expect(fs.existsSync(pidPath)).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // detectAndHandlePathMismatch — path normalization
  // -------------------------------------------------------------------------

  describe("detectAndHandlePathMismatch (path normalization)", () => {
    it("detects mismatch with different directory paths", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      writePidFile(pidPath, {
        pid: 99999,
        startedAtMs: Date.now(),
        entryPath: "/Applications/Crab.app/Contents/Resources/daemon/main.js",
      });

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath: path.join(tmpDir, "daemon-v1.sock"),
        tokenPath: path.join(tmpDir, "daemon-v1.token"),
        currentEntryPath: "/Applications/Crab-2.0.app/Contents/Resources/daemon/main.js",
        isProcessAlive: () => false,
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(true);
    });

    it("treats paths with duplicate slashes as equivalent", () => {
      const pidPath = path.join(tmpDir, "daemon-v1.pid");
      writePidFile(pidPath, {
        pid: 12345,
        startedAtMs: Date.now(),
        entryPath: "/app//v1///daemon-main.js",
      });

      const opts: PathMismatchDetectionOptions = {
        pidPath,
        socketPath: path.join(tmpDir, "daemon-v1.sock"),
        tokenPath: path.join(tmpDir, "daemon-v1.token"),
        currentEntryPath: "/app/v1/daemon-main.js",
      };

      const result = detectAndHandlePathMismatch(opts);

      expect(result.mismatch).toBe(false);
    });
  });
});
