import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { AuditLogger, type AuditLoggerOptions } from "../electron/daemon/audit-logger";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

interface MockFs {
  files: Map<string, string>;
  dirs: Set<string>;
  renames: Array<{ from: string; to: string }>;
}

function createMockFs(): MockFs {
  return {
    files: new Map(),
    dirs: new Set(),
    renames: [],
  };
}

function createLogger(mockFs: MockFs, opts?: Partial<AuditLoggerOptions>): AuditLogger {
  return new AuditLogger({
    filePath: opts?.filePath ?? "/tmp/audit.log",
    maxFileSizeBytes: opts?.maxFileSizeBytes ?? 50 * 1024 * 1024,
    maxRotatedFiles: opts?.maxRotatedFiles ?? 5,
    getFileSize: (filePath: string) => {
      const content = mockFs.files.get(filePath);
      if (content === undefined) throw new Error("ENOENT");
      return Buffer.byteLength(content, "utf-8");
    },
    fileExists: (filePath: string) => mockFs.files.has(filePath),
    renameFile: (oldPath: string, newPath: string) => {
      const content = mockFs.files.get(oldPath);
      if (content !== undefined) {
        mockFs.files.set(newPath, content);
        mockFs.files.delete(oldPath);
      }
      mockFs.renames.push({ from: oldPath, to: newPath });
    },
    unlinkFile: (filePath: string) => {
      mockFs.files.delete(filePath);
    },
    appendFile: (filePath: string, data: string) => {
      const existing = mockFs.files.get(filePath) ?? "";
      mockFs.files.set(filePath, existing + data);
    },
    mkdirSync: (dirPath: string) => {
      mockFs.dirs.add(dirPath);
    },
  });
}

function getLogEntries(mockFs: MockFs, filePath = "/tmp/audit.log"): Array<Record<string, unknown>> {
  const content = mockFs.files.get(filePath) ?? "";
  return content
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("AuditLogger", () => {
  let mockFs: MockFs;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-15T10:30:00.000Z"));
    mockFs = createMockFs();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // -------------------------------------------------------------------------
  // Enabled / disabled behavior
  // -------------------------------------------------------------------------

  describe("enabled/disabled", () => {
    it("is enabled when filePath is provided", () => {
      const logger = createLogger(mockFs);
      expect(logger.enabled).toBe(true);
    });

    it("is disabled when filePath is not set", () => {
      const logger = new AuditLogger({ filePath: undefined });
      expect(logger.enabled).toBe(false);
    });

    it("is disabled when filePath is empty string", () => {
      const logger = createLogger(mockFs, { filePath: "" });
      expect(logger.enabled).toBe(false);
    });

    it("is disabled when filePath is whitespace only", () => {
      const logger = createLogger(mockFs, { filePath: "   " });
      expect(logger.enabled).toBe(false);
    });

    it("is disabled after dispose", () => {
      const logger = createLogger(mockFs);
      expect(logger.enabled).toBe(true);
      logger.dispose();
      expect(logger.enabled).toBe(false);
    });

    it("creates parent directory on construction", () => {
      createLogger(mockFs, { filePath: "/var/log/crab/audit.log" });
      expect(mockFs.dirs.has("/var/log/crab")).toBe(true);
    });
  });

  // -------------------------------------------------------------------------
  // No-op when disabled
  // -------------------------------------------------------------------------

  describe("no-op when disabled", () => {
    it("does not write when filePath is not set", () => {
      const logger = new AuditLogger({ filePath: undefined });
      logger.sessionCreated("s1");
      // No crash, no file written
    });

    it("does not write after dispose", () => {
      const logger = createLogger(mockFs);
      logger.sessionCreated("s1");
      expect(mockFs.files.has("/tmp/audit.log")).toBe(true);

      logger.dispose();
      mockFs.files.clear();

      logger.sessionCreated("s2");
      expect(mockFs.files.has("/tmp/audit.log")).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // Session lifecycle events
  // -------------------------------------------------------------------------

  describe("session lifecycle events", () => {
    it("logs session.created with correct fields", () => {
      const logger = createLogger(mockFs);
      logger.sessionCreated("ws1@@abc123", { cwd: "/home/user", command: "bash" });

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]).toEqual({
        ts: "2026-01-15T10:30:00.000Z",
        event: "session.created",
        sessionId: "ws1@@abc123",
        cwd: "/home/user",
        command: "bash",
      });
    });

    it("logs session.attached", () => {
      const logger = createLogger(mockFs);
      logger.sessionAttached("s1", { clientId: "client-1" });

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]).toMatchObject({
        event: "session.attached",
        sessionId: "s1",
        clientId: "client-1",
      });
    });

    it("logs session.detached", () => {
      const logger = createLogger(mockFs);
      logger.sessionDetached("s1", { clientId: "client-1" });

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]).toMatchObject({
        event: "session.detached",
        sessionId: "s1",
        clientId: "client-1",
      });
    });

    it("logs session.killed", () => {
      const logger = createLogger(mockFs);
      logger.sessionKilled("s1", { reason: "user_request" });

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]).toMatchObject({
        event: "session.killed",
        sessionId: "s1",
        reason: "user_request",
      });
    });

    it("logs session.exited", () => {
      const logger = createLogger(mockFs);
      logger.sessionExited("s1", { exitCode: 0 });

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]).toMatchObject({
        event: "session.exited",
        sessionId: "s1",
        exitCode: 0,
      });
    });
  });

  // -------------------------------------------------------------------------
  // Agent state transitions
  // -------------------------------------------------------------------------

  describe("agent state transitions", () => {
    it("logs agent.state_changed with state and previousState", () => {
      const logger = createLogger(mockFs);
      logger.agentStateChanged("s1", "working", "idle", { taskId: "task-42" });

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]).toEqual({
        ts: "2026-01-15T10:30:00.000Z",
        event: "agent.state_changed",
        sessionId: "s1",
        state: "working",
        previousState: "idle",
        taskId: "task-42",
      });
    });

    it("logs multiple state transitions in order", () => {
      const logger = createLogger(mockFs);
      logger.agentStateChanged("s1", "working", "idle");
      logger.agentStateChanged("s1", "waiting", "working");
      logger.agentStateChanged("s1", "exited", "waiting");

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(3);
      expect(entries[0]!.state).toBe("working");
      expect(entries[1]!.state).toBe("waiting");
      expect(entries[2]!.state).toBe("exited");
    });
  });

  // -------------------------------------------------------------------------
  // JSON-lines format
  // -------------------------------------------------------------------------

  describe("JSON-lines format", () => {
    it("writes one JSON object per line", () => {
      const logger = createLogger(mockFs);
      logger.sessionCreated("s1");
      logger.sessionCreated("s2");

      const content = mockFs.files.get("/tmp/audit.log") ?? "";
      const lines = content.split("\n").filter((l) => l.length > 0);
      expect(lines).toHaveLength(2);

      // Each line is valid JSON
      for (const line of lines) {
        expect(() => JSON.parse(line)).not.toThrow();
      }
    });

    it("includes ISO 8601 timestamp", () => {
      const logger = createLogger(mockFs);
      logger.sessionCreated("s1");

      const entries = getLogEntries(mockFs);
      expect(entries[0]!.ts).toBe("2026-01-15T10:30:00.000Z");
    });
  });

  // -------------------------------------------------------------------------
  // File rotation
  // -------------------------------------------------------------------------

  describe("file rotation", () => {
    it("rotates when file exceeds max size", () => {
      const logger = createLogger(mockFs, { maxFileSizeBytes: 100 });

      // Seed the file with content exceeding 100 bytes
      mockFs.files.set("/tmp/audit.log", "x".repeat(101));

      logger.sessionCreated("s1");

      // The old file should have been rotated to .1
      expect(mockFs.files.has("/tmp/audit.log.1")).toBe(true);
      expect(mockFs.files.get("/tmp/audit.log.1")).toBe("x".repeat(101));

      // New entry should be in the main file
      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
      expect(entries[0]!.event).toBe("session.created");
    });

    it("shifts existing rotated files up", () => {
      const logger = createLogger(mockFs, { maxFileSizeBytes: 100 });

      // Set up existing rotated files
      mockFs.files.set("/tmp/audit.log", "x".repeat(101));
      mockFs.files.set("/tmp/audit.log.1", "old-1");
      mockFs.files.set("/tmp/audit.log.2", "old-2");

      logger.sessionCreated("s1");

      expect(mockFs.files.get("/tmp/audit.log.3")).toBe("old-2");
      expect(mockFs.files.get("/tmp/audit.log.2")).toBe("old-1");
      expect(mockFs.files.get("/tmp/audit.log.1")).toBe("x".repeat(101));
    });

    it("removes the oldest file when at max rotated files", () => {
      const logger = createLogger(mockFs, { maxFileSizeBytes: 100, maxRotatedFiles: 3 });

      mockFs.files.set("/tmp/audit.log", "x".repeat(101));
      mockFs.files.set("/tmp/audit.log.1", "old-1");
      mockFs.files.set("/tmp/audit.log.2", "old-2");
      mockFs.files.set("/tmp/audit.log.3", "oldest");

      logger.sessionCreated("s1");

      // .3 (oldest) should be gone, replaced by what was .2
      expect(mockFs.files.get("/tmp/audit.log.3")).toBe("old-2");
      expect(mockFs.files.get("/tmp/audit.log.2")).toBe("old-1");
      expect(mockFs.files.get("/tmp/audit.log.1")).toBe("x".repeat(101));
      // "oldest" content is gone
    });

    it("does not rotate when file is below max size", () => {
      const logger = createLogger(mockFs, { maxFileSizeBytes: 1000 });

      mockFs.files.set("/tmp/audit.log", "small");

      logger.sessionCreated("s1");

      expect(mockFs.files.has("/tmp/audit.log.1")).toBe(false);
      // Entry appended to existing file
      const content = mockFs.files.get("/tmp/audit.log") ?? "";
      expect(content.startsWith("small")).toBe(true);
    });

    it("handles rotation when file does not exist yet", () => {
      const logger = createLogger(mockFs, { maxFileSizeBytes: 100 });

      // No file exists — should not throw
      logger.sessionCreated("s1");

      const entries = getLogEntries(mockFs);
      expect(entries).toHaveLength(1);
    });
  });

  // -------------------------------------------------------------------------
  // Error resilience
  // -------------------------------------------------------------------------

  describe("error resilience", () => {
    it("does not throw when appendFile fails", () => {
      const logger = new AuditLogger({
        filePath: "/tmp/audit.log",
        appendFile: () => {
          throw new Error("disk full");
        },
        getFileSize: () => {
          throw new Error("ENOENT");
        },
        fileExists: () => false,
        renameFile: () => {},
        unlinkFile: () => {},
        mkdirSync: () => {},
      });

      // Should not throw
      expect(() => logger.sessionCreated("s1")).not.toThrow();
    });

    it("does not throw when rotation fails", () => {
      const logger = new AuditLogger({
        filePath: "/tmp/audit.log",
        getFileSize: () => 100 * 1024 * 1024, // over limit
        fileExists: () => true,
        renameFile: () => {
          throw new Error("permission denied");
        },
        unlinkFile: () => {
          throw new Error("permission denied");
        },
        appendFile: () => {},
        mkdirSync: () => {},
      });

      expect(() => logger.sessionCreated("s1")).not.toThrow();
    });
  });
});
