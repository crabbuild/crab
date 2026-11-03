/**
 * Unit tests for the SshProfileManager class.
 *
 * Tests CRUD operations, sorting, validation, keychain integration
 * (mocked), and export/import functionality.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

// Mock the keychain module before importing the profile manager
vi.mock("../electron/keychain", () => ({
  getPassword: vi.fn().mockResolvedValue(null),
  setPassword: vi.fn().mockResolvedValue(undefined),
  deletePassword: vi.fn().mockResolvedValue(true),
}));

import { SshProfileManager } from "../electron/ssh-profiles";
import { getPassword, setPassword, deletePassword } from "../electron/keychain";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeTempDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "crab-ssh-profiles-test-"));
}

function createManager(tmpDir: string): SshProfileManager {
  const profilesPath = path.join(tmpDir, "ssh-profiles.json");
  return new SshProfileManager(profilesPath);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("SshProfileManager", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = makeTempDir();
    vi.clearAllMocks();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  describe("CRUD operations", () => {
    it("list() returns empty array when no profiles exist", () => {
      const manager = createManager(tmpDir);
      expect(manager.list()).toEqual([]);
    });

    it("save() creates a new profile with generated ID", () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "server.example.com", username: "deploy" });

      expect(profile.id).toBeDefined();
      expect(profile.id).toMatch(/^[0-9a-f-]{36}$/);
      expect(profile.host).toBe("server.example.com");
      expect(profile.username).toBe("deploy");
      expect(profile.port).toBe(22);
      expect(profile.label).toBe("server.example.com");
      expect(profile.createdAt).toBeGreaterThan(0);
    });

    it("save() applies default values for optional fields", () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "myhost.io", username: "user" });

      expect(profile.forwardAgent).toBe(false);
      expect(profile.compression).toBe(false);
      expect(profile.keepAliveInterval).toBe(15);
      expect(profile.autoReconnect).toBe(true);
      expect(profile.autoDeployAgent).toBe(true);
      expect(profile.tags).toEqual([]);
      expect(profile.portForwards).toEqual([]);
      expect(profile.authMethod).toEqual({ type: "agent" });
    });

    it("save() updates an existing profile", () => {
      const manager = createManager(tmpDir);
      const created = manager.save({ host: "server.example.com", username: "deploy" });

      const updated = manager.save({
        id: created.id,
        host: "server.example.com",
        username: "deploy",
        port: 2222,
        label: "My Server",
      });

      expect(updated.id).toBe(created.id);
      expect(updated.port).toBe(2222);
      expect(updated.label).toBe("My Server");
      expect(manager.list()).toHaveLength(1);
    });

    it("save() throws when updating a non-existent profile", () => {
      const manager = createManager(tmpDir);

      expect(() =>
        manager.save({ id: "non-existent-id", host: "server.example.com", username: "user" }),
      ).toThrow("Profile not found");
    });

    it("delete() removes a profile and returns true", async () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "server.example.com", username: "deploy" });

      const result = await manager.delete(profile.id);

      expect(result).toBe(true);
      expect(manager.list()).toHaveLength(0);
    });

    it("delete() returns false for non-existent profile", async () => {
      const manager = createManager(tmpDir);

      const result = await manager.delete("non-existent-id");

      expect(result).toBe(false);
    });

    it("get() returns a profile by ID", () => {
      const manager = createManager(tmpDir);
      const created = manager.save({ host: "server.example.com", username: "deploy" });

      const found = manager.get(created.id);

      expect(found).toBeDefined();
      expect(found!.id).toBe(created.id);
      expect(found!.host).toBe("server.example.com");
    });

    it("get() returns undefined for non-existent ID", () => {
      const manager = createManager(tmpDir);

      expect(manager.get("non-existent-id")).toBeUndefined();
    });
  });

  describe("sorting", () => {
    it("pinned profiles appear first", () => {
      const manager = createManager(tmpDir);
      manager.save({ host: "unpinned.example.com", username: "user" });
      manager.save({ host: "pinned.example.com", username: "user", pinned: true });

      const list = manager.list();

      expect(list[0].host).toBe("pinned.example.com");
      expect(list[1].host).toBe("unpinned.example.com");
    });

    it("within same pin group, sorts by lastConnectedAt descending", () => {
      const manager = createManager(tmpDir);
      manager.save({ host: "old.example.com", username: "user", lastConnectedAt: 1000 });
      manager.save({ host: "recent.example.com", username: "user", lastConnectedAt: 5000 });
      manager.save({ host: "middle.example.com", username: "user", lastConnectedAt: 3000 });

      const list = manager.list();

      expect(list[0].host).toBe("recent.example.com");
      expect(list[1].host).toBe("middle.example.com");
      expect(list[2].host).toBe("old.example.com");
    });

    it("profiles without lastConnectedAt sort last", () => {
      const manager = createManager(tmpDir);
      manager.save({ host: "never-connected.example.com", username: "user" });
      manager.save({ host: "connected.example.com", username: "user", lastConnectedAt: 1000 });

      const list = manager.list();

      expect(list[0].host).toBe("connected.example.com");
      expect(list[1].host).toBe("never-connected.example.com");
    });
  });

  describe("validation", () => {
    it("rejects empty host", () => {
      const manager = createManager(tmpDir);

      expect(() => manager.save({ host: "", username: "user" })).toThrow("Host is required");
    });

    it("rejects host with spaces", () => {
      const manager = createManager(tmpDir);

      expect(() => manager.save({ host: "my server", username: "user" })).toThrow(
        "Host must not contain spaces",
      );
    });

    it("accepts valid hostname", () => {
      const manager = createManager(tmpDir);

      const profile = manager.save({ host: "my-server.example.com", username: "user" });
      expect(profile.host).toBe("my-server.example.com");
    });

    it("accepts valid IPv4 address", () => {
      const manager = createManager(tmpDir);

      const profile = manager.save({ host: "192.168.1.100", username: "user" });
      expect(profile.host).toBe("192.168.1.100");
    });

    it("rejects invalid port (0)", () => {
      const manager = createManager(tmpDir);

      expect(() => manager.save({ host: "server.example.com", username: "user", port: 0 })).toThrow(
        "Port must be an integer between 1 and 65535",
      );
    });

    it("rejects invalid port (65536)", () => {
      const manager = createManager(tmpDir);

      expect(() =>
        manager.save({ host: "server.example.com", username: "user", port: 65536 }),
      ).toThrow("Port must be an integer between 1 and 65535");
    });

    it("accepts valid port range boundaries", () => {
      const manager = createManager(tmpDir);

      const p1 = manager.save({ host: "server1.example.com", username: "user", port: 1 });
      expect(p1.port).toBe(1);

      const p2 = manager.save({ host: "server2.example.com", username: "user", port: 65535 });
      expect(p2.port).toBe(65535);
    });

    it("rejects empty username", () => {
      const manager = createManager(tmpDir);

      expect(() => manager.save({ host: "server.example.com", username: "" })).toThrow(
        "Username is required",
      );
    });

    it("rejects key auth with missing key file", () => {
      const manager = createManager(tmpDir);

      expect(() =>
        manager.save({
          host: "server.example.com",
          username: "user",
          authMethod: { type: "key", keyPath: "/nonexistent/path/id_rsa" },
        }),
      ).toThrow("Key file not found or not readable");
    });

    it("accepts key auth with existing key file", () => {
      const manager = createManager(tmpDir);
      const keyPath = path.join(tmpDir, "test_key");
      fs.writeFileSync(keyPath, "fake-key-content", "utf-8");

      const profile = manager.save({
        host: "server.example.com",
        username: "user",
        authMethod: { type: "key", keyPath },
      });

      expect(profile.authMethod).toEqual({ type: "key", keyPath });
    });
  });

  describe("keychain integration", () => {
    it("setCredential stores password in keychain", async () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "server.example.com", username: "user" });

      await manager.setCredential(profile.id, "my-secret-password");

      expect(setPassword).toHaveBeenCalledWith("crab-desktop-ssh", profile.id, "my-secret-password");
    });

    it("getCredential retrieves password from keychain", async () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "server.example.com", username: "user" });
      vi.mocked(getPassword).mockResolvedValueOnce("stored-password");

      const result = await manager.getCredential(profile.id);

      expect(getPassword).toHaveBeenCalledWith("crab-desktop-ssh", profile.id);
      expect(result).toBe("stored-password");
    });

    it("deleteCredential removes password from keychain", async () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "server.example.com", username: "user" });

      await manager.deleteCredential(profile.id);

      expect(deletePassword).toHaveBeenCalledWith("crab-desktop-ssh", profile.id);
    });

    it("delete() cleans up keychain entries", async () => {
      const manager = createManager(tmpDir);
      const profile = manager.save({ host: "server.example.com", username: "user" });

      await manager.delete(profile.id);

      expect(deletePassword).toHaveBeenCalledWith("crab-desktop-ssh", profile.id);
    });
  });

  describe("export/import", () => {
    it("exportProfiles produces valid JSON with version and profiles", () => {
      const manager = createManager(tmpDir);
      manager.save({ host: "server1.example.com", username: "user1" });
      manager.save({ host: "server2.example.com", username: "user2" });

      const exported = manager.exportProfiles();
      const parsed = JSON.parse(exported);

      expect(parsed.version).toBe(1);
      expect(parsed.exportedAt).toBeGreaterThan(0);
      expect(parsed.profiles).toHaveLength(2);
      expect(parsed.profiles[0].host).toBe("server1.example.com");
      expect(parsed.profiles[1].host).toBe("server2.example.com");
    });

    it("exportProfiles can export a subset by IDs", () => {
      const manager = createManager(tmpDir);
      const p1 = manager.save({ host: "server1.example.com", username: "user1" });
      manager.save({ host: "server2.example.com", username: "user2" });

      const exported = manager.exportProfiles([p1.id]);
      const parsed = JSON.parse(exported);

      expect(parsed.profiles).toHaveLength(1);
      expect(parsed.profiles[0].host).toBe("server1.example.com");
    });

    it("importProfiles generates new IDs for imported profiles", () => {
      const manager = createManager(tmpDir);
      const original = manager.save({ host: "server.example.com", username: "user" });
      const exported = manager.exportProfiles();

      // Create a fresh manager and import
      const manager2 = createManager(makeTempDir());
      const imported = manager2.importProfiles(exported);

      expect(imported).toHaveLength(1);
      expect(imported[0].id).not.toBe(original.id);
      expect(imported[0].host).toBe("server.example.com");
      expect(imported[0].username).toBe("user");
    });

    it("importProfiles resets lastConnectedAt", () => {
      const manager = createManager(tmpDir);
      manager.save({ host: "server.example.com", username: "user", lastConnectedAt: 12345 });
      const exported = manager.exportProfiles();

      const manager2 = createManager(makeTempDir());
      const imported = manager2.importProfiles(exported);

      expect(imported[0].lastConnectedAt).toBeUndefined();
    });

    it("importProfiles skips profiles that fail validation", () => {
      const manager = createManager(tmpDir);
      const invalidJson = JSON.stringify({
        version: 1,
        exportedAt: Date.now(),
        profiles: [
          { host: "", username: "user" }, // invalid: empty host
          { host: "valid.example.com", username: "user" }, // valid
        ],
      });

      const imported = manager.importProfiles(invalidJson);

      expect(imported).toHaveLength(1);
      expect(imported[0].host).toBe("valid.example.com");
    });

    it("importProfiles throws on invalid JSON", () => {
      const manager = createManager(tmpDir);

      expect(() => manager.importProfiles("not json")).toThrow("Invalid JSON format");
    });

    it("importProfiles throws on missing profiles array", () => {
      const manager = createManager(tmpDir);

      expect(() => manager.importProfiles(JSON.stringify({ version: 1 }))).toThrow(
        "missing profiles array",
      );
    });
  });

  describe("persistence", () => {
    it("persists profiles across manager instances", () => {
      const profilesPath = path.join(tmpDir, "ssh-profiles.json");
      const manager1 = new SshProfileManager(profilesPath);
      manager1.save({ host: "server.example.com", username: "user" });

      const manager2 = new SshProfileManager(profilesPath);
      const list = manager2.list();

      expect(list).toHaveLength(1);
      expect(list[0].host).toBe("server.example.com");
    });

    it("creates parent directories if they don't exist", () => {
      const deepPath = path.join(tmpDir, "a", "b", "c", "ssh-profiles.json");
      const manager = new SshProfileManager(deepPath);
      manager.save({ host: "server.example.com", username: "user" });

      expect(fs.existsSync(deepPath)).toBe(true);
    });

    it("handles corrupted JSON gracefully", () => {
      const profilesPath = path.join(tmpDir, "ssh-profiles.json");
      fs.writeFileSync(profilesPath, "not valid json {{{", "utf-8");

      const manager = new SshProfileManager(profilesPath);

      expect(manager.list()).toHaveLength(0);
    });
  });
});
