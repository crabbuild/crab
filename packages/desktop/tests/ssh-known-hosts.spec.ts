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
import { utils } from "ssh2";
import { KnownHostsManager, type HostKeyRequest } from "../electron/ssh-known-hosts";

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
      expect(payload.asciiArt).toContain("+----[SSH key]----+");
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
      expect(lines[0]).toBe("+----[SSH key]----+");
      expect(lines[10]).toBe("+----[SHA256]-----+");

      // Each row is 19 chars: | + 17 chars + |
      for (let i = 1; i <= 9; i++) {
        expect(lines[i]).toHaveLength(19);
        expect(lines[i][0]).toBe("|");
        expect(lines[i][18]).toBe("|");
      }
    });

    it("matches OpenSSH SHA256 fingerprint and randomart for a real generated public key", () => {
      const { manager } = createManager(tmpDir);
      // Public fixture and rows captured with ssh-keygen -lv -E sha256.
      const key = Buffer.from("AAAAB3NzaC1yc2EAAAADAQABAAABAQC2ao/BAbWDxkts+DGKuMFxkOah+tinRGbn+7yYCeCwL6CJiifI58mcMvd0m5UgGvFwGFc1FJ5e44U3c7sZ7e1/GffGEBF1nPuVdYMsIk5BPb4xlUJd2rXqkGMjcy9oOwHWCBXXj9li6e3aFGUp2aP7PVHu3UsFK+HKfc+1PFVPkyrkZwBc8t/xUGQVrX3vT3rmvVL75DlxzadiM4KXjwRCNtskFZk+hQWX8UPPsQiS3xewylgiMVZyR2nUz/7EKARSxGRvZR4TZXDd2flUW7M8BnzatRbXATV9YQJpxOx9HquI4DW7+2nqNCJcAZzuTvZ39pVqoM42Y2KUS63UCXl/", "base64");
      const rows = [
      "|        oo.o.o.  |",
      "|        .=... o..|",
      "|          * + .+.|",
      "|         = * =..+|",
      "|        S O @.o+o|",
      "|         o O.E+.=|",
      "|          o oo.O.|",
      "|           .. o +|",
      "|          ..   ..|"
];
      expect({ fingerprint: manager.computeFingerprint(key), rows: manager.generateAsciiArt(key).split("\n").slice(1, -1) }).toEqual({ fingerprint: "SHA256:5RcbhuzDuxdVb7ol1trU7gUdDGLVqnkAScZvbwmalz0", rows });
    });

    it("produces different art for different keys", () => {
      const { manager } = createManager(tmpDir);
      const key1 = generateTestKey();
      const key2 = generateTestKey();

      const art1 = manager.generateAsciiArt(key1);
      const art2 = manager.generateAsciiArt(key2);

      expect(art1).not.toBe(art2);
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
  it("does not extend a port-22 system trust entry to another port", async () => {
    const key = generateTestKey();
    const { manager } = createManager(tmpDir, `server.example.com ssh-ed25519 ${key.toString("base64")}\n`);
    expect(await manager.verify("server.example.com", 2222, key, "ssh-ed25519")).toBe("unknown");
  });

  it("does not let an older system key bypass an explicitly accepted app pin", async () => {
    const old = generateTestKey(), current = generateTestKey();
    const { manager } = createManager(tmpDir, `server.example.com ssh-ed25519 ${old.toString("base64")}\n`);
    await manager.accept("server.example.com", 22, current, "ssh-ed25519");
    expect(await manager.verify("server.example.com", 22, old, "ssh-ed25519")).toBe("changed");
  });

  it("recognizes an OpenSSH hashed host at its exact port", async () => {
    const key = generateTestKey();
    // Host field generated by OpenSSH ssh-keygen -H for this exact endpoint.
    const hashed = "|1|Z19HXE1zxJCYWFGshjlgfOIrNJU=|2JXSjQWxAgYlJDh/hf5075aXefs=";
    const { manager } = createManager(tmpDir, `${hashed} ssh-ed25519 ${key.toString("base64")}\n`);
    expect(await manager.verify("server.example.com", 2222, key, "ssh-ed25519")).toBe("trusted");
  });

  it("rejects a revoked key even when another entry would trust it", async () => {
    const key = generateTestKey();
    const line = `server.example.com ssh-ed25519 ${key.toString("base64")}\n`;
    const { manager } = createManager(tmpDir, line + "@revoked " + line);
    await expect(manager.verify("server.example.com", 22, key, "ssh-ed25519")).rejects.toThrow(/revoked/);
  });

  function realKey(): Buffer {
    const pair = crypto.generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs1", format: "pem" }, publicKeyEncoding: { type: "pkcs1", format: "pem" } });
    const key = utils.parseKey(pair.privateKey);
    if (key instanceof Error) throw key;
    return key.getPublicSSH();
  }

  it("persists explicit trust before resolving authentication and reuses the saved pin", async () => {
    const { manager, hostsPath, systemPath } = createManager(tmpDir);
    const key = realKey();
    const prompts: HostKeyRequest[] = [];
    manager.on("ssh:host-key-request", prompt => prompts.push(prompt));
    const result = manager.authorize("server.example.com", 2222, key, new AbortController().signal);
    expect(prompts[0]).toMatchObject({ state: "unknown", fingerprint: manager.computeFingerprint(key), keyType: "ssh-rsa" });
    await manager.answer(prompts[0].requestId, true);
    expect(await result).toBe(true);
    expect(await new KnownHostsManager(hostsPath, systemPath).verify("server.example.com", 2222, key, "ssh-rsa")).toBe("trusted");
  });

  it("aborted trust requests cannot be accepted by a late UI response", async () => {
    const { manager, hostsPath } = createManager(tmpDir);
    const prompts: HostKeyRequest[] = [];
    manager.on("ssh:host-key-request", prompt => prompts.push(prompt));
    const controller = new AbortController();
    const result = manager.authorize("server.example.com", 22, realKey(), controller.signal);
    controller.abort();
    await manager.answer(prompts[0].requestId, true);
    expect(await result).toBe(false);
    expect(fs.existsSync(hostsPath)).toBe(false);
  });

  it("failed persistence rejects authentication without trusting the key in memory", async () => {
    const { manager, hostsPath } = createManager(tmpDir);
    const prompts: HostKeyRequest[] = [];
    manager.on("ssh:host-key-request", prompt => prompts.push(prompt));
    const result = manager.authorize("server.example.com", 22, realKey(), new AbortController().signal);
    fs.mkdirSync(hostsPath);
    await expect(manager.answer(prompts[0].requestId, true)).rejects.toMatchObject({ code: "EISDIR" });
    expect(await result).toBe(false);
    expect(manager.list()).toHaveLength(0);
  });

  it("does not overwrite a corrupt trust store when accepting a host", async () => {
    const hostsPath = path.join(tmpDir, "known_hosts.json");
    fs.writeFileSync(hostsPath, "corrupt data");
    const manager = new KnownHostsManager(hostsPath, path.join(tmpDir, "missing-system"));
    await expect(manager.accept("server.example.com", 22, realKey(), "ssh-rsa")).rejects.toThrow("Cannot read SSH host trust store");
    expect(fs.readFileSync(hostsPath, "utf8")).toBe("corrupt data");
  });

});
