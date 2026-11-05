/**
 * Unit tests for the KnownHostsManager class.
 *
 * Tests host key verification, acceptance, persistence, system
 * known_hosts fallback, and event emission for unknown/changed hosts.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { KnownHostsManager } from "../electron/ssh-known-hosts";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function generateTestKey(): Buffer {
  return crypto.randomBytes(256);
}

function makeTempDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "crab-known-hosts-test-"));
}

function createManager(
  tmpDir: string,
  systemKnownHostsContent?: string,
): { manager: KnownHostsManager; hostsPath: string; systemPath: string } {
  const hostsPath = path.join(tmpDir, "known_hosts.json");
  const systemPath = path.join(tmpDir, "system_known_hosts");

  if (systemKnownHostsContent !== undefined) {
    fs.writeFileSync(systemPath, systemKnownHostsContent, "utf-8");
  }

  const manager = new KnownHostsManager(hostsPath, systemPath);
  return { manager, hostsPath, systemPath };
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("KnownHostsManager", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = makeTempDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  describe("verify", () => {
    it("returns 'unknown' for a host never seen before", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      const result = await manager.verify("new-host.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("unknown");
    });

    it("returns 'trusted' for a host with matching fingerprint", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      await manager.accept("server.example.com", 22, key, "ssh-ed25519");
      const result = await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("trusted");
    });

    it("returns 'changed' when host key differs from stored", async () => {
      const { manager } = createManager(tmpDir);
      const originalKey = generateTestKey();
      const newKey = generateTestKey();

      await manager.accept("server.example.com", 22, originalKey, "ssh-ed25519");
      const result = await manager.verify("server.example.com", 22, newKey, "ssh-ed25519");

      expect(result).toBe("changed");
    });

    it("distinguishes hosts by port", async () => {
      const { manager } = createManager(tmpDir);
      const key22 = generateTestKey();
      const key2222 = generateTestKey();

      await manager.accept("server.example.com", 22, key22, "ssh-ed25519");
      await manager.accept("server.example.com", 2222, key2222, "ssh-ed25519");

      expect(await manager.verify("server.example.com", 22, key22, "ssh-ed25519")).toBe("trusted");
      expect(await manager.verify("server.example.com", 2222, key2222, "ssh-ed25519")).toBe("trusted");
      expect(await manager.verify("server.example.com", 22, key2222, "ssh-ed25519")).toBe("changed");
    });

    it("emits ssh:host-key-prompt for unknown hosts", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();
      const events: unknown[] = [];

      manager.on("ssh:host-key-prompt", (payload) => events.push(payload));
      await manager.verify("new-host.example.com", 22, key, "ssh-ed25519");

      expect(events).toHaveLength(1);
      const payload = events[0] as Record<string, unknown>;
      expect(payload.hostname).toBe("new-host.example.com");
      expect(payload.port).toBe(22);
      expect(payload.keyType).toBe("ssh-ed25519");
      expect(payload.fingerprint).toMatch(/^SHA256:/);
      expect(payload.asciiArt).toContain("+---[HASH]----+");
    });

    it("emits ssh:host-key-changed for mismatched keys", async () => {
      const { manager } = createManager(tmpDir);
      const originalKey = generateTestKey();
      const newKey = generateTestKey();
      const events: unknown[] = [];

      await manager.accept("server.example.com", 22, originalKey, "ssh-ed25519");
      manager.on("ssh:host-key-changed", (payload) => events.push(payload));
      await manager.verify("server.example.com", 22, newKey, "ssh-ed25519");

      expect(events).toHaveLength(1);
      const payload = events[0] as Record<string, unknown>;
      expect(payload.hostname).toBe("server.example.com");
      expect(payload.port).toBe(22);
      expect(payload.fingerprint).toMatch(/^SHA256:/);
      expect(payload.oldFingerprint).toMatch(/^SHA256:/);
      expect(payload.fingerprint).not.toBe(payload.oldFingerprint);
    });

    it("does not emit events for trusted hosts", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();
      const events: unknown[] = [];

      await manager.accept("server.example.com", 22, key, "ssh-ed25519");
      manager.on("ssh:host-key-prompt", (p) => events.push(p));
      manager.on("ssh:host-key-changed", (p) => events.push(p));
      await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(events).toHaveLength(0);
    });
  });

  describe("accept", () => {
    it("stores a new host entry", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      await manager.accept("server.example.com", 22, key, "ssh-rsa");

      const hosts = manager.list();
      expect(hosts).toHaveLength(1);
      expect(hosts[0].hostname).toBe("server.example.com");
      expect(hosts[0].port).toBe(22);
      expect(hosts[0].keyType).toBe("ssh-rsa");
      expect(hosts[0].fingerprint).toMatch(/^SHA256:/);
      expect(hosts[0].addedAt).toBeGreaterThan(0);
    });

    it("replaces an existing entry for the same host:port", async () => {
      const { manager } = createManager(tmpDir);
      const key1 = generateTestKey();
      const key2 = generateTestKey();

      await manager.accept("server.example.com", 22, key1, "ssh-rsa");
      await manager.accept("server.example.com", 22, key2, "ssh-ed25519");

      const hosts = manager.list();
      expect(hosts).toHaveLength(1);
      expect(hosts[0].keyType).toBe("ssh-ed25519");
      expect(hosts[0].fingerprint).toBe(manager.computeFingerprint(key2));
    });
  });

  describe("remove", () => {
    it("removes an existing host entry", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      await manager.accept("server.example.com", 22, key, "ssh-ed25519");
      const removed = manager.remove("server.example.com", 22);

      expect(removed).toBe(true);
      expect(manager.list()).toHaveLength(0);
    });

    it("returns false when host not found", () => {
      const { manager } = createManager(tmpDir);

      const removed = manager.remove("nonexistent.example.com", 22);

      expect(removed).toBe(false);
    });
  });

  describe("persistence", () => {
    it("persists hosts across manager instances", async () => {
      const hostsPath = path.join(tmpDir, "known_hosts.json");
      const systemPath = path.join(tmpDir, "system_known_hosts");
      const key = generateTestKey();

      const manager1 = new KnownHostsManager(hostsPath, systemPath);
      await manager1.accept("server.example.com", 22, key, "ssh-ed25519");

      // Create a new instance that reads from the same file
      const manager2 = new KnownHostsManager(hostsPath, systemPath);
      const result = await manager2.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("trusted");
    });

    it("creates parent directories if they don't exist", async () => {
      const deepPath = path.join(tmpDir, "a", "b", "c", "known_hosts.json");
      const systemPath = path.join(tmpDir, "system_known_hosts");
      const key = generateTestKey();

      const manager = new KnownHostsManager(deepPath, systemPath);
      await manager.accept("server.example.com", 22, key, "ssh-ed25519");

      expect(fs.existsSync(deepPath)).toBe(true);
    });

    it("handles corrupted JSON gracefully", () => {
      const hostsPath = path.join(tmpDir, "known_hosts.json");
      const systemPath = path.join(tmpDir, "system_known_hosts");
      fs.writeFileSync(hostsPath, "not valid json {{{", "utf-8");

      const manager = new KnownHostsManager(hostsPath, systemPath);

      expect(manager.list()).toHaveLength(0);
    });
  });

  describe("system known_hosts fallback", () => {
    it("trusts a host found in system known_hosts", async () => {
      const key = generateTestKey();
      const keyBase64 = key.toString("base64");
      const systemContent = `server.example.com ssh-ed25519 ${keyBase64}\n`;

      const { manager } = createManager(tmpDir, systemContent);
      const result = await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("trusted");
    });

    it("handles non-standard port in system known_hosts", async () => {
      const key = generateTestKey();
      const keyBase64 = key.toString("base64");
      const systemContent = `[server.example.com]:2222 ssh-ed25519 ${keyBase64}\n`;

      const { manager } = createManager(tmpDir, systemContent);
      const result = await manager.verify("server.example.com", 2222, key, "ssh-ed25519");

      expect(result).toBe("trusted");
    });

    it("does not emit events when system known_hosts matches", async () => {
      const key = generateTestKey();
      const keyBase64 = key.toString("base64");
      const systemContent = `server.example.com ssh-ed25519 ${keyBase64}\n`;
      const events: unknown[] = [];

      const { manager } = createManager(tmpDir, systemContent);
      manager.on("ssh:host-key-prompt", (p) => events.push(p));
      manager.on("ssh:host-key-changed", (p) => events.push(p));
      await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(events).toHaveLength(0);
    });

    it("returns 'unknown' when system known_hosts does not contain the host", async () => {
      const key = generateTestKey();
      const systemContent = `other-host.example.com ssh-ed25519 ${Buffer.from("other-key").toString("base64")}\n`;

      const { manager } = createManager(tmpDir, systemContent);
      const result = await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("unknown");
    });

    it("handles missing system known_hosts file gracefully", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      // No system file created — should not throw
      const result = await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("unknown");
    });

    it("skips comment lines and empty lines in system known_hosts", async () => {
      const key = generateTestKey();
      const keyBase64 = key.toString("base64");
      const systemContent = [
        "# This is a comment",
        "",
        `server.example.com ssh-ed25519 ${keyBase64}`,
        "# Another comment",
        "",
      ].join("\n");

      const { manager } = createManager(tmpDir, systemContent);
      const result = await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("trusted");
    });

    it("handles multiple hosts on the same line (comma-separated)", async () => {
      const key = generateTestKey();
      const keyBase64 = key.toString("base64");
      const systemContent = `server.example.com,192.168.1.100 ssh-ed25519 ${keyBase64}\n`;

      const { manager } = createManager(tmpDir, systemContent);
      const result = await manager.verify("192.168.1.100", 22, key, "ssh-ed25519");

      expect(result).toBe("trusted");
    });

    it("skips hashed entries (|1| prefix)", async () => {
      const key = generateTestKey();
      const keyBase64 = key.toString("base64");
      // Hashed entries can't be matched without the HMAC key
      const systemContent = `|1|abc123|def456 ssh-ed25519 ${keyBase64}\n`;

      const { manager } = createManager(tmpDir, systemContent);
      const result = await manager.verify("server.example.com", 22, key, "ssh-ed25519");

      expect(result).toBe("unknown");
    });
  });

  describe("computeFingerprint", () => {
    it("produces consistent fingerprints for the same key", () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      const fp1 = manager.computeFingerprint(key);
      const fp2 = manager.computeFingerprint(key);

      expect(fp1).toBe(fp2);
    });

    it("produces different fingerprints for different keys", () => {
      const { manager } = createManager(tmpDir);
      const key1 = generateTestKey();
      const key2 = generateTestKey();

      const fp1 = manager.computeFingerprint(key1);
      const fp2 = manager.computeFingerprint(key2);

      expect(fp1).not.toBe(fp2);
    });

    it("prefixes fingerprint with SHA256:", () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      const fp = manager.computeFingerprint(key);

      expect(fp).toMatch(/^SHA256:[A-Za-z0-9+/]+=*$/);
    });
  });

  describe("generateAsciiArt", () => {
    it("produces a valid randomart grid", () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      const art = manager.generateAsciiArt(key);
      const lines = art.split("\n");

      // 11 lines: top border + 9 rows + bottom border
      expect(lines).toHaveLength(11);
      expect(lines[0]).toBe("+---[HASH]----+");
      expect(lines[10]).toBe("+-------------+");

      // Each row is 19 chars: | + 17 chars + |
      for (let i = 1; i <= 9; i++) {
        expect(lines[i]).toHaveLength(19);
        expect(lines[i][0]).toBe("|");
        expect(lines[i][18]).toBe("|");
      }
    });

    it("produces different art for different keys", () => {
      const { manager } = createManager(tmpDir);
      const key1 = generateTestKey();
      const key2 = generateTestKey();

      const art1 = manager.generateAsciiArt(key1);
      const art2 = manager.generateAsciiArt(key2);

      expect(art1).not.toBe(art2);
    });

    it("contains start (S) and end (E) markers", () => {
      const { manager } = createManager(tmpDir);
      // The walk can return to its starting cell. Use a key whose endpoints differ.
      const key = Buffer.from("crab-known-host-randomart-fixture");

      const art = manager.generateAsciiArt(key);

      expect(art).toContain("S");
      expect(art).toContain("E");
    });
  });

  describe("list", () => {
    it("returns empty array when no hosts stored", () => {
      const { manager } = createManager(tmpDir);

      expect(manager.list()).toEqual([]);
    });

    it("returns a copy (mutations don't affect internal state)", async () => {
      const { manager } = createManager(tmpDir);
      const key = generateTestKey();

      await manager.accept("server.example.com", 22, key, "ssh-ed25519");
      const list = manager.list();
      list.pop();

      expect(manager.list()).toHaveLength(1);
    });
  });
});
