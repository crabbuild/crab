import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import { HistoryManager, sanitizeEnv, getSanitizedKeys } from "../electron/daemon/history-manager";
import type { TerminalSnapshot } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "history-manager-test-"));
}

function makeSnapshot(overrides?: Partial<TerminalSnapshot>): TerminalSnapshot {
  return {
    snapshotAnsi: "$ hello world\r\n",
    scrollbackAnsi: "",
    rehydrateSequences: "",
    cwd: "/home/user",
    modes: {
      bracketedPaste: false,
      mouseTracking: false,
      mouseTrackingMode: "none",
      sgrMouseMode: false,
      applicationCursor: false,
      alternateScreen: false,
    },
    cols: 80,
    rows: 24,
    scrollbackLines: 0,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("HistoryManager", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = createTmpDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  // -------------------------------------------------------------------------
  // Atomic write
  // -------------------------------------------------------------------------

  describe("atomic write", () => {
    it("writes checkpoint.json atomically (no tmp file left behind)", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot());

      const sessionDir = path.join(tmpDir, "sess-1");
      const files = fs.readdirSync(sessionDir);

      // Should have meta.json and checkpoint.json, no .tmp files
      expect(files).toContain("checkpoint.json");
      expect(files).toContain("meta.json");
      expect(files.filter((f) => f.includes(".tmp"))).toHaveLength(0);
    });

    it("checkpoint.json contains valid snapshot data", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");

      const snapshot = makeSnapshot({ cwd: "/home/user/project" });
      hm.checkpoint("sess-1", snapshot);

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      const raw = fs.readFileSync(checkpointPath, "utf-8");
      const data = JSON.parse(raw);

      expect(data.snapshot.cwd).toBe("/home/user/project");
      expect(data.snapshot.cols).toBe(80);
      expect(data.snapshot.rows).toBe(24);
      expect(data.checkpointedAt).toBeDefined();
    });

    it("overwrites previous checkpoint atomically", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });

      // First checkpoint
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot({ cwd: "/first" }));

      // Second checkpoint
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot({ cwd: "/second" }));

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      const raw = fs.readFileSync(checkpointPath, "utf-8");
      const data = JSON.parse(raw);

      expect(data.snapshot.cwd).toBe("/second");
    });
  });

  // -------------------------------------------------------------------------
  // Cold restore detection
  // -------------------------------------------------------------------------

  describe("cold restore detection", () => {
    it("detects unclean shutdown (meta without endedAt + checkpoint exists)", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");

      const snapshot = makeSnapshot({ snapshotAnsi: "recovered content" });
      hm.checkpoint("sess-1", snapshot);

      // Simulate daemon crash — create a new HistoryManager instance
      // (the old one didn't call closeSession)
      const hm2 = new HistoryManager({ basePath: tmpDir });
      const restored = hm2.detectColdRestore("sess-1");

      expect(restored).not.toBeNull();
      expect(restored!.snapshotAnsi).toBe("recovered content");
    });

    it("returns null for cleanly closed session", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot());
      hm.closeSession("sess-1", 0);

      const hm2 = new HistoryManager({ basePath: tmpDir });
      const restored = hm2.detectColdRestore("sess-1");

      expect(restored).toBeNull();
    });

    it("returns null when no checkpoint exists", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      // No checkpoint written

      const hm2 = new HistoryManager({ basePath: tmpDir });
      const restored = hm2.detectColdRestore("sess-1");

      expect(restored).toBeNull();
    });

    it("returns null for non-existent session", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      const restored = hm.detectColdRestore("nonexistent");

      expect(restored).toBeNull();
    });
  });

  // -------------------------------------------------------------------------
  // Pruning
  // -------------------------------------------------------------------------

  describe("pruning", () => {
    it("removes directories older than maxAgeDays", () => {
      const hm = new HistoryManager({ basePath: tmpDir, maxAgeDays: 7 });

      // Create a session directory with old mtime
      const oldDir = path.join(tmpDir, "old-session");
      fs.mkdirSync(oldDir, { recursive: true });
      fs.writeFileSync(path.join(oldDir, "meta.json"), "{}");

      // Set mtime to 8 days ago
      const eightDaysAgo = new Date(Date.now() - 8 * 24 * 60 * 60 * 1000);
      fs.utimesSync(oldDir, eightDaysAgo, eightDaysAgo);

      // Create a recent session directory
      const recentDir = path.join(tmpDir, "recent-session");
      fs.mkdirSync(recentDir, { recursive: true });
      fs.writeFileSync(path.join(recentDir, "meta.json"), "{}");

      hm.prune();

      expect(fs.existsSync(oldDir)).toBe(false);
      expect(fs.existsSync(recentDir)).toBe(true);
    });

    it("enforces size cap by removing oldest directories first", () => {
      // 50 bytes cap — very small for testing
      const hm = new HistoryManager({ basePath: tmpDir, maxSizeBytes: 50 });

      // Create directories with known sizes
      const dir1 = path.join(tmpDir, "sess-1");
      fs.mkdirSync(dir1);
      fs.writeFileSync(path.join(dir1, "data.json"), "x".repeat(30));
      // Set older mtime
      const twoDaysAgo = new Date(Date.now() - 2 * 24 * 60 * 60 * 1000);
      fs.utimesSync(dir1, twoDaysAgo, twoDaysAgo);

      const dir2 = path.join(tmpDir, "sess-2");
      fs.mkdirSync(dir2);
      fs.writeFileSync(path.join(dir2, "data.json"), "y".repeat(30));
      // Set slightly newer mtime
      const oneDayAgo = new Date(Date.now() - 1 * 24 * 60 * 60 * 1000);
      fs.utimesSync(dir2, oneDayAgo, oneDayAgo);

      // Total: 60 bytes > 50 byte cap
      hm.prune();

      // Oldest (sess-1) should be removed to get under cap
      expect(fs.existsSync(dir1)).toBe(false);
      expect(fs.existsSync(dir2)).toBe(true);
    });

    it("does nothing when basePath does not exist", () => {
      const hm = new HistoryManager({ basePath: path.join(tmpDir, "nonexistent") });
      // Should not throw
      expect(() => hm.prune()).not.toThrow();
    });
  });

  // -------------------------------------------------------------------------
  // Environment sanitization
  // -------------------------------------------------------------------------

  describe("env sanitization", () => {
    it("strips known secret variables", () => {
      const env = {
        PATH: "/usr/bin",
        HOME: "/home/user",
        AWS_SECRET_ACCESS_KEY: "secret123",
        GITHUB_TOKEN: "ghp_abc123",
        ANTHROPIC_API_KEY: "sk-ant-abc",
        SHELL: "/bin/zsh",
      };

      const sanitized = sanitizeEnv(env)!;

      expect(sanitized.PATH).toBe("/usr/bin");
      expect(sanitized.HOME).toBe("/home/user");
      expect(sanitized.SHELL).toBe("/bin/zsh");
      expect(sanitized.AWS_SECRET_ACCESS_KEY).toBeUndefined();
      expect(sanitized.GITHUB_TOKEN).toBeUndefined();
      expect(sanitized.ANTHROPIC_API_KEY).toBeUndefined();
    });

    it("strips variables matching *_SECRET* pattern", () => {
      const env = {
        MY_SECRET_VALUE: "hidden",
        DATABASE_SECRET_KEY: "hidden",
        NORMAL_VAR: "visible",
      };

      const sanitized = sanitizeEnv(env)!;

      expect(sanitized.MY_SECRET_VALUE).toBeUndefined();
      expect(sanitized.DATABASE_SECRET_KEY).toBeUndefined();
      expect(sanitized.NORMAL_VAR).toBe("visible");
    });

    it("strips variables matching *_TOKEN* pattern", () => {
      const env = {
        NPM_TOKEN: "npm_abc",
        AUTH_TOKEN_VALUE: "hidden",
        NORMAL_VAR: "visible",
      };

      const sanitized = sanitizeEnv(env)!;

      expect(sanitized.NPM_TOKEN).toBeUndefined();
      expect(sanitized.AUTH_TOKEN_VALUE).toBeUndefined();
      expect(sanitized.NORMAL_VAR).toBe("visible");
    });

    it("strips variables matching *_KEY* pattern", () => {
      const env = {
        API_KEY: "hidden",
        ENCRYPTION_KEY_ID: "hidden",
        KEYBOARD: "visible",
      };

      const sanitized = sanitizeEnv(env)!;

      expect(sanitized.API_KEY).toBeUndefined();
      expect(sanitized.ENCRYPTION_KEY_ID).toBeUndefined();
      // KEYBOARD doesn't match _KEY pattern (no underscore before KEY)
      expect(sanitized.KEYBOARD).toBe("visible");
    });

    it("returns undefined for undefined input", () => {
      expect(sanitizeEnv(undefined)).toBeUndefined();
    });

    it("persisted meta.json does not contain secrets", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        env: {
          PATH: "/usr/bin",
          AWS_SECRET_ACCESS_KEY: "secret",
          GITHUB_TOKEN: "ghp_token",
          MY_API_KEY: "key123",
          NORMAL: "value",
        },
      });

      const metaPath = path.join(tmpDir, "sess-1", "meta.json");
      const raw = fs.readFileSync(metaPath, "utf-8");
      const meta = JSON.parse(raw);

      expect(meta.env.PATH).toBe("/usr/bin");
      expect(meta.env.NORMAL).toBe("value");
      expect(meta.env.AWS_SECRET_ACCESS_KEY).toBeUndefined();
      expect(meta.env.GITHUB_TOKEN).toBeUndefined();
      expect(meta.env.MY_API_KEY).toBeUndefined();
    });
  });

  // -------------------------------------------------------------------------
  // Sensitive session flag
  // -------------------------------------------------------------------------

  describe("sensitive flag", () => {
    it("skips checkpoint entirely for sensitive sessions", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("ssh-sess", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        sensitive: true,
      });
      hm.markDirty("ssh-sess");
      hm.checkpoint("ssh-sess", makeSnapshot());

      const checkpointPath = path.join(tmpDir, "ssh-sess", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);
    });

    it("still writes meta.json for sensitive sessions", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("ssh-sess", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        sensitive: true,
      });

      const metaPath = path.join(tmpDir, "ssh-sess", "meta.json");
      expect(fs.existsSync(metaPath)).toBe(true);

      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.sensitive).toBe(true);
    });

    it("non-sensitive sessions write checkpoints normally", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("normal-sess", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        sensitive: false,
      });
      hm.markDirty("normal-sess");
      hm.checkpoint("normal-sess", makeSnapshot());

      const checkpointPath = path.join(tmpDir, "normal-sess", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);
    });
  });

  // -------------------------------------------------------------------------
  // Dirty-version tracking
  // -------------------------------------------------------------------------

  describe("dirty-version tracking", () => {
    it("skips checkpoint when session is not dirty", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      // Don't mark dirty
      hm.checkpoint("sess-1", makeSnapshot());

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);
    });

    it("writes checkpoint when session is dirty", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot());

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);
    });

    it("does not re-checkpoint after version is consumed", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot({ cwd: "/first" }));

      // Second checkpoint without marking dirty — should be skipped
      hm.checkpoint("sess-1", makeSnapshot({ cwd: "/second" }));

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      const data = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(data.snapshot.cwd).toBe("/first");
    });
  });

  // -------------------------------------------------------------------------
  // Error handling
  // -------------------------------------------------------------------------

  describe("error handling", () => {
    it("disables on failure and continues operating", () => {
      // Use a path that will cause permission errors
      const hm = new HistoryManager({ basePath: "/dev/null/impossible" });

      // Should not throw — just disables
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      expect(hm.isDisabled).toBe(true);

      // Subsequent operations are no-ops
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot());
      hm.closeSession("sess-1", 0);
      // None of these should throw
    });
  });

  // -------------------------------------------------------------------------
  // Session lifecycle
  // -------------------------------------------------------------------------

  describe("session lifecycle", () => {
    it("openSession creates directory and meta.json", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/home/user", cols: 120, rows: 40 });

      const metaPath = path.join(tmpDir, "sess-1", "meta.json");
      expect(fs.existsSync(metaPath)).toBe(true);

      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.sessionId).toBe("sess-1");
      expect(meta.cwd).toBe("/home/user");
      expect(meta.cols).toBe(120);
      expect(meta.rows).toBe(40);
      expect(meta.createdAt).toBeDefined();
    });

    it("closeSession updates meta with endedAt and exitCode", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.closeSession("sess-1", 42);

      const metaPath = path.join(tmpDir, "sess-1", "meta.json");
      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.endedAt).toBeDefined();
      expect(meta.exitCode).toBe(42);
    });

    it("removeSession deletes the directory", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("sess-1");
      hm.checkpoint("sess-1", makeSnapshot());

      const sessionDir = path.join(tmpDir, "sess-1");
      expect(fs.existsSync(sessionDir)).toBe(true);

      hm.removeSession("sess-1");
      expect(fs.existsSync(sessionDir)).toBe(false);
    });

    it("dispose marks all sessions as ended", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.openSession("sess-2", { cwd: "/tmp", cols: 80, rows: 24 });

      hm.dispose();

      const meta1 = JSON.parse(
        fs.readFileSync(path.join(tmpDir, "sess-1", "meta.json"), "utf-8"),
      );
      const meta2 = JSON.parse(
        fs.readFileSync(path.join(tmpDir, "sess-2", "meta.json"), "utf-8"),
      );

      expect(meta1.endedAt).toBeDefined();
      expect(meta2.endedAt).toBeDefined();
    });
  });

  // -------------------------------------------------------------------------
  // getSanitizedKeys
  // -------------------------------------------------------------------------

  describe("getSanitizedKeys", () => {
    it("returns keys that would be stripped by sanitizeEnv", () => {
      const env = {
        PATH: "/usr/bin",
        AWS_SECRET_ACCESS_KEY: "secret",
        GITHUB_TOKEN: "ghp_abc",
        NORMAL: "value",
      };
      const keys = getSanitizedKeys(env);
      expect(keys).toContain("AWS_SECRET_ACCESS_KEY");
      expect(keys).toContain("GITHUB_TOKEN");
      expect(keys).not.toContain("PATH");
      expect(keys).not.toContain("NORMAL");
    });

    it("returns empty array for undefined env", () => {
      expect(getSanitizedKeys(undefined)).toEqual([]);
    });

    it("returns empty array when no secrets present", () => {
      const env = { PATH: "/usr/bin", HOME: "/home/user", SHELL: "/bin/zsh" };
      expect(getSanitizedKeys(env)).toEqual([]);
    });

    it("matches the keys absent from sanitizeEnv output", () => {
      const env = {
        PATH: "/usr/bin",
        MY_API_KEY: "hidden",
        NPM_TOKEN: "hidden",
        NORMAL: "value",
      };
      const sanitized = sanitizeEnv(env)!;
      const stripped = getSanitizedKeys(env);
      for (const key of stripped) {
        expect(sanitized[key]).toBeUndefined();
      }
      for (const key of Object.keys(sanitized)) {
        expect(stripped).not.toContain(key);
      }
    });
  });

  // -------------------------------------------------------------------------
  // readMeta
  // -------------------------------------------------------------------------

  describe("readMeta", () => {
    it("returns null for non-existent session", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      expect(hm.readMeta("nonexistent")).toBeNull();
    });

    it("returns meta for an open session", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", { cwd: "/home/user", cols: 120, rows: 40 });

      const meta = hm.readMeta("sess-1");
      expect(meta).not.toBeNull();
      expect(meta!.sessionId).toBe("sess-1");
      expect(meta!.cwd).toBe("/home/user");
      expect(meta!.cols).toBe(120);
      expect(meta!.rows).toBe(40);
    });

    it("persists sanitizedKeys in meta.json and returns them via readMeta", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        env: {
          PATH: "/usr/bin",
          AWS_SECRET_ACCESS_KEY: "secret",
          GITHUB_TOKEN: "ghp_abc",
          NORMAL: "value",
        },
        sanitizedKeys: ["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"],
      });

      const meta = hm.readMeta("sess-1");
      expect(meta).not.toBeNull();
      expect(meta!.sanitizedKeys).toEqual(["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"]);
    });

    it("returns empty sanitizedKeys array for sessions without secrets", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        env: { PATH: "/usr/bin", NORMAL: "value" },
        sanitizedKeys: [],
      });

      const meta = hm.readMeta("sess-1");
      expect(meta!.sanitizedKeys).toEqual([]);
    });

    it("returns empty sanitizedKeys array for older meta.json without the field", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      // Write a meta.json without sanitizedKeys (simulating older format)
      const sessionDir = path.join(tmpDir, "old-sess");
      fs.mkdirSync(sessionDir, { recursive: true });
      fs.writeFileSync(
        path.join(sessionDir, "meta.json"),
        JSON.stringify({
          sessionId: "old-sess",
          cwd: "/tmp",
          cols: 80,
          rows: 24,
          createdAt: new Date().toISOString(),
        }),
      );

      const meta = hm.readMeta("old-sess");
      expect(meta).not.toBeNull();
      expect(meta!.sanitizedKeys).toEqual([]);
    });

    it("env in meta.json is sanitized (secrets stripped)", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("sess-1", {
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        env: {
          PATH: "/usr/bin",
          AWS_SECRET_ACCESS_KEY: "secret",
          NORMAL: "value",
        },
        sanitizedKeys: ["AWS_SECRET_ACCESS_KEY"],
      });

      const meta = hm.readMeta("sess-1");
      expect(meta!.env!.PATH).toBe("/usr/bin");
      expect(meta!.env!.NORMAL).toBe("value");
      expect(meta!.env!.AWS_SECRET_ACCESS_KEY).toBeUndefined();
    });

    it("returns null for malformed meta.json", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      const sessionDir = path.join(tmpDir, "bad-sess");
      fs.mkdirSync(sessionDir, { recursive: true });
      fs.writeFileSync(path.join(sessionDir, "meta.json"), "not valid json{{{");

      expect(hm.readMeta("bad-sess")).toBeNull();
    });
  });
});
