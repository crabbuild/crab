/**
 * Tests for SSH audit log — structured connection event logging
 * with rotation and credential scrubbing.
 *
 * Verifies:
 * - write() appends structured JSON entries to the log file
 * - Entries contain timestamp, event type, profileId, and optional details
 * - Log rotation triggers at 10MB (renames to .1, starts fresh)
 * - Sensitive fields (password, passphrase, privateKey) are scrubbed
 * - PEM-encoded key content in values is detected and redacted
 * - Nested objects have sensitive fields scrubbed recursively
 * - readEntries() returns parsed entries from the log
 * - Graceful handling of missing/corrupted log files
 * - Parent directories are created if they don't exist
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { EventEmitter } from "node:events";
import { SshAuditLog, AuditEntry, AuditEventType, bridgeAuditLog } from "../electron/ssh-audit-log";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createTempLogPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-audit-test-"));
  return path.join(dir, "ssh-audit.log");
}

function cleanupPath(filePath: string): void {
  try {
    const dir = path.dirname(filePath);
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup
  }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("SshAuditLog", () => {
  let logPath: string;
  let auditLog: SshAuditLog;

  beforeEach(() => {
    logPath = createTempLogPath();
    auditLog = new SshAuditLog(logPath);
  });

  afterEach(() => {
    cleanupPath(logPath);
  });

  describe("write", () => {
    it("appends a structured JSON entry to the log file", () => {
      auditLog.write("connect", "profile-1", { host: "gpu-01.internal" });

      const content = fs.readFileSync(logPath, "utf-8");
      const lines = content.trim().split("\n");
      expect(lines).toHaveLength(1);

      const entry: AuditEntry = JSON.parse(lines[0]);
      expect(entry.event).toBe("connect");
      expect(entry.profileId).toBe("profile-1");
      expect(entry.details).toEqual({ host: "gpu-01.internal" });
      expect(entry.timestamp).toMatch(/^\d{4}-\d{2}-\d{2}T/);
    });

    it("writes entries for all supported event types", () => {
      const events: AuditEventType[] = [
        "connect",
        "disconnect",
        "auth_failure",
        "reconnection",
        "deploy",
        "deploy_failed",
      ];

      for (const event of events) {
        auditLog.write(event, "profile-1");
      }

      const entries = auditLog.readEntries();
      expect(entries).toHaveLength(6);
      expect(entries.map((e) => e.event)).toEqual(events);
    });

    it("appends multiple entries sequentially", () => {
      auditLog.write("connect", "profile-1");
      auditLog.write("disconnect", "profile-1");
      auditLog.write("connect", "profile-2");

      const entries = auditLog.readEntries();
      expect(entries).toHaveLength(3);
      expect(entries[0].event).toBe("connect");
      expect(entries[1].event).toBe("disconnect");
      expect(entries[2].profileId).toBe("profile-2");
    });

    it("omits details field when no details are provided", () => {
      auditLog.write("connect", "profile-1");

      const content = fs.readFileSync(logPath, "utf-8");
      const entry = JSON.parse(content.trim());
      expect(entry.details).toBeUndefined();
    });

    it("omits details field when details is an empty object", () => {
      auditLog.write("connect", "profile-1", {});

      const content = fs.readFileSync(logPath, "utf-8");
      const entry = JSON.parse(content.trim());
      expect(entry.details).toBeUndefined();
    });

    it("creates parent directories if they do not exist", () => {
      const deepPath = path.join(
        os.tmpdir(),
        `crab-audit-deep-${Date.now()}`,
        "nested",
        "dir",
        "ssh-audit.log",
      );
      const deepLog = new SshAuditLog(deepPath);

      deepLog.write("connect", "profile-1");

      expect(fs.existsSync(deepPath)).toBe(true);
      const entries = deepLog.readEntries();
      expect(entries).toHaveLength(1);

      // Cleanup
      fs.rmSync(path.dirname(path.dirname(path.dirname(deepPath))), {
        recursive: true,
        force: true,
      });
    });
  });

  describe("credential scrubbing", () => {
    it("redacts password fields", () => {
      auditLog.write("auth_failure", "profile-1", {
        host: "gpu-01.internal",
        password: "super-secret-123",
        username: "admin",
      });

      const entries = auditLog.readEntries();
      expect(entries[0].details!.password).toBe("[REDACTED]");
      expect(entries[0].details!.host).toBe("gpu-01.internal");
      expect(entries[0].details!.username).toBe("admin");
    });

    it("redacts passphrase fields", () => {
      auditLog.write("auth_failure", "profile-1", {
        passphrase: "my-key-passphrase",
        keyPath: "/home/user/.ssh/id_ed25519",
      });

      const entries = auditLog.readEntries();
      expect(entries[0].details!.passphrase).toBe("[REDACTED]");
      expect(entries[0].details!.keyPath).toBe("/home/user/.ssh/id_ed25519");
    });

    it("redacts privateKey fields", () => {
      auditLog.write("auth_failure", "profile-1", {
        privateKey: "-----BEGIN OPENSSH PRIVATE KEY-----\nbase64data\n-----END OPENSSH PRIVATE KEY-----",
      });

      const entries = auditLog.readEntries();
      expect(entries[0].details!.privateKey).toBe("[REDACTED]");
    });

    it("redacts values that look like PEM private keys", () => {
      auditLog.write("auth_failure", "profile-1", {
        someField: "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAK...\n-----END RSA PRIVATE KEY-----",
      });

      const entries = auditLog.readEntries();
      expect(entries[0].details!.someField).toBe("[REDACTED]");
    });

    it("redacts sensitive fields in nested objects", () => {
      auditLog.write("auth_failure", "profile-1", {
        auth: {
          method: "key_passphrase",
          passphrase: "secret-phrase",
          keyPath: "/home/user/.ssh/id_rsa",
        },
      });

      const entries = auditLog.readEntries();
      const auth = entries[0].details!.auth as Record<string, unknown>;
      expect(auth.passphrase).toBe("[REDACTED]");
      expect(auth.method).toBe("key_passphrase");
      expect(auth.keyPath).toBe("/home/user/.ssh/id_rsa");
    });

    it("is case-insensitive for sensitive key detection", () => {
      auditLog.write("auth_failure", "profile-1", {
        Password: "secret",
        SECRET: "value",
        Token: "abc123",
      });

      const entries = auditLog.readEntries();
      expect(entries[0].details!.Password).toBe("[REDACTED]");
      expect(entries[0].details!.SECRET).toBe("[REDACTED]");
      expect(entries[0].details!.Token).toBe("[REDACTED]");
    });

    it("preserves non-sensitive fields unchanged", () => {
      auditLog.write("connect", "profile-1", {
        host: "gpu-01.internal",
        port: 22,
        username: "admin",
        latencyMs: 45,
        tags: ["gpu", "training"],
      });

      const entries = auditLog.readEntries();
      expect(entries[0].details).toEqual({
        host: "gpu-01.internal",
        port: 22,
        username: "admin",
        latencyMs: 45,
        tags: ["gpu", "training"],
      });
    });
  });

  describe("log rotation", () => {
    it("rotates the log file when it exceeds 10MB", () => {
      // Write a large amount of data to exceed 10MB
      const largeEntry = "x".repeat(1024); // 1KB per detail value
      const entriesNeeded = Math.ceil((10 * 1024 * 1024) / 100) + 10;

      // Create a file that's already at the limit
      const bigContent = "a".repeat(10 * 1024 * 1024 + 1);
      fs.writeFileSync(logPath, bigContent, "utf-8");

      // Write a new entry — should trigger rotation
      auditLog.write("connect", "profile-1", { host: "gpu-01" });

      // The old content should be in the rotated file
      const rotatedPath = logPath + ".1";
      expect(fs.existsSync(rotatedPath)).toBe(true);
      const rotatedContent = fs.readFileSync(rotatedPath, "utf-8");
      expect(rotatedContent).toBe(bigContent);

      // The new log should only have the new entry
      const newContent = fs.readFileSync(logPath, "utf-8");
      const entry = JSON.parse(newContent.trim());
      expect(entry.event).toBe("connect");
    });

    it("does not rotate when file is under 10MB", () => {
      auditLog.write("connect", "profile-1");
      auditLog.write("disconnect", "profile-1");

      const rotatedPath = logPath + ".1";
      expect(fs.existsSync(rotatedPath)).toBe(false);
      expect(auditLog.readEntries()).toHaveLength(2);
    });

    it("overwrites previous rotation file", () => {
      const rotatedPath = logPath + ".1";

      // Create an existing rotated file
      fs.writeFileSync(rotatedPath, "old-rotated-content", "utf-8");

      // Create a file at the limit
      const bigContent = "b".repeat(10 * 1024 * 1024 + 1);
      fs.writeFileSync(logPath, bigContent, "utf-8");

      // Trigger rotation
      auditLog.write("connect", "profile-1");

      // The rotated file should have the new content, not the old
      const rotatedContent = fs.readFileSync(rotatedPath, "utf-8");
      expect(rotatedContent).toBe(bigContent);
    });
  });

  describe("readEntries", () => {
    it("returns parsed entries from the log file", () => {
      auditLog.write("connect", "profile-1", { host: "server-a" });
      auditLog.write("disconnect", "profile-2");

      const entries = auditLog.readEntries();
      expect(entries).toHaveLength(2);
      expect(entries[0].event).toBe("connect");
      expect(entries[0].profileId).toBe("profile-1");
      expect(entries[1].event).toBe("disconnect");
      expect(entries[1].profileId).toBe("profile-2");
    });

    it("returns empty array when log file does not exist", () => {
      const freshLog = new SshAuditLog(path.join(os.tmpdir(), "nonexistent", "audit.log"));
      expect(freshLog.readEntries()).toEqual([]);
    });

    it("skips malformed lines gracefully", () => {
      // Write a mix of valid and invalid lines
      const validEntry = JSON.stringify({
        timestamp: "2024-01-01T00:00:00.000Z",
        event: "connect",
        profileId: "profile-1",
      });
      const content = `${validEntry}\nnot-valid-json\n${validEntry}\n`;
      fs.writeFileSync(logPath, content, "utf-8");

      const entries = auditLog.readEntries();
      expect(entries).toHaveLength(2);
    });

    it("returns empty array for an empty file", () => {
      fs.writeFileSync(logPath, "", "utf-8");
      expect(auditLog.readEntries()).toEqual([]);
    });
  });

  describe("getLogPath / getRotatedPath", () => {
    it("returns the configured log path", () => {
      expect(auditLog.getLogPath()).toBe(logPath);
    });

    it("returns the rotated log path with .1 suffix", () => {
      expect(auditLog.getRotatedPath()).toBe(logPath + ".1");
    });
  });

  describe("default path", () => {
    it("uses ~/.crab/desktop/ssh-audit.log when no path is provided", () => {
      const defaultLog = new SshAuditLog();
      const expected = path.join(os.homedir(), ".crab", "desktop", "ssh-audit.log");
      expect(defaultLog.getLogPath()).toBe(expected);
    });
  });
});

describe("bridgeAuditLog", () => {
  let logPath: string;
  let auditLog: SshAuditLog;
  let sessionManager: EventEmitter;
  let deployer: EventEmitter;

  beforeEach(() => {
    logPath = createTempLogPath();
    auditLog = new SshAuditLog(logPath);
    sessionManager = new EventEmitter();
    deployer = new EventEmitter();
    bridgeAuditLog(auditLog, sessionManager, deployer);
  });

  afterEach(() => {
    cleanupPath(logPath);
  });

  it("logs connect event when status changes to connected", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "connected",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("connect");
    expect(entries[0].profileId).toBe("gpu-server");
  });

  it("logs disconnect event when status changes to disconnected", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "disconnected",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("disconnect");
    expect(entries[0].profileId).toBe("gpu-server");
  });

  it("logs disconnect with reason when error is provided", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "disconnected",
      error: "Connection timed out",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("disconnect");
    expect(entries[0].details).toEqual({ reason: "Connection timed out" });
  });

  it("logs auth_failure when error status indicates authentication failure", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "error",
      error: "Authentication failed",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("auth_failure");
    expect(entries[0].profileId).toBe("gpu-server");
    expect(entries[0].details).toEqual({ error: "Authentication failed" });
  });

  it("logs auth_failure for permission denied errors", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "error",
      error: "Permission denied (publickey)",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("auth_failure");
  });

  it("does not log non-auth errors as auth_failure", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "error",
      error: "Host unreachable",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(0);
  });

  it("logs reconnection event when status changes to reconnecting", () => {
    sessionManager.emit("ssh:status-changed", {
      profileId: "gpu-server",
      status: "reconnecting",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("reconnection");
    expect(entries[0].profileId).toBe("gpu-server");
  });

  it("logs deploy event when agent deployment completes", () => {
    deployer.emit("ssh:deploy-progress", {
      stage: "starting",
      percent: 100,
      profileId: "gpu-server",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("deploy");
    expect(entries[0].profileId).toBe("gpu-server");
  });

  it("does not log intermediate deploy progress events", () => {
    deployer.emit("ssh:deploy-progress", {
      stage: "uploading",
      percent: 50,
      profileId: "gpu-server",
    });
    deployer.emit("ssh:deploy-progress", {
      stage: "verifying",
      percent: 100,
      profileId: "gpu-server",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(0);
  });

  it("logs deploy_failed event", () => {
    deployer.emit("ssh:deploy-failed", {
      profileId: "gpu-server",
      error: "Disk full on remote server",
    });

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].event).toBe("deploy_failed");
    expect(entries[0].profileId).toBe("gpu-server");
    expect(entries[0].details).toEqual({ error: "Disk full on remote server" });
  });

  it("does not crash when deployer is not provided", () => {
    const logPath2 = createTempLogPath();
    const auditLog2 = new SshAuditLog(logPath2);
    const sm = new EventEmitter();

    // Should not throw
    bridgeAuditLog(auditLog2, sm);

    sm.emit("ssh:status-changed", { profileId: "test", status: "connected" });
    const entries = auditLog2.readEntries();
    expect(entries).toHaveLength(1);

    cleanupPath(logPath2);
  });

  it("handles rapid successive events without data loss", () => {
    for (let i = 0; i < 20; i++) {
      sessionManager.emit("ssh:status-changed", {
        profileId: `profile-${i}`,
        status: "connected",
      });
    }

    const entries = auditLog.readEntries();
    expect(entries).toHaveLength(20);
  });
});
