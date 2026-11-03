/**
 * Tests for SSH session persistence — saving active connections on quit
 * and restoring them on restart with autoReconnect filtering.
 *
 * Verifies:
 * - saveState writes active connection profile IDs to the state file
 * - Only "connected" and "reconnecting" connections are persisted
 * - getProfileIdsToReconnect filters by autoReconnect and profile existence
 * - Corrupted or missing state files are handled gracefully (empty array)
 * - clearState removes the state file
 * - removeProfile removes a single profile from persisted state
 * - State file format matches { version: 1, activeConnections: [...], savedAt: number }
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { SessionPersistence, SshSessionState } from "../electron/ssh-session-persistence";
import type { SshProfile, SshConnection, SshConnectionStatus } from "../electron/ssh-manager";

// ─── Mock SSH Session Manager ────────────────────────────────────────────────

class MockSshSessionManager extends EventEmitter {
  private connections = new Map<string, MockConnection>();

  addConnection(profileId: string, status: SshConnectionStatus, autoReconnect = true): void {
    this.connections.set(profileId, {
      profile: createMockProfile(profileId, autoReconnect),
      status,
      client: {} as any,
      channels: { terminals: new Map(), portForwards: new Map() },
      jumpClients: [],
    });
  }

  getAllConnections(): MockConnection[] {
    return Array.from(this.connections.values());
  }

  getConnection(profileId: string): MockConnection | undefined {
    return this.connections.get(profileId);
  }

  getStatus(profileId: string): SshConnectionStatus {
    return this.connections.get(profileId)?.status ?? "disconnected";
  }
}

interface MockConnection {
  profile: SshProfile;
  status: SshConnectionStatus;
  client: any;
  channels: any;
  jumpClients: any[];
}

// ─── Mock SSH Profile Manager ────────────────────────────────────────────────

class MockSshProfileManager {
  private profiles = new Map<string, SshProfile>();

  addProfile(profileId: string, autoReconnect = true): void {
    this.profiles.set(profileId, createMockProfile(profileId, autoReconnect));
  }

  get(profileId: string): SshProfile | undefined {
    return this.profiles.get(profileId);
  }

  list(): SshProfile[] {
    return Array.from(this.profiles.values());
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createMockProfile(id: string, autoReconnect = true): SshProfile {
  return {
    id,
    label: `Server ${id}`,
    host: `${id}.example.com`,
    port: 22,
    username: "user",
    authMethod: { type: "agent" },
    forwardAgent: false,
    compression: false,
    keepAliveInterval: 15,
    autoReconnect,
    autoDeployAgent: true,
    tags: [],
    portForwards: [],
    createdAt: Date.now(),
  };
}

function createTempStatePath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-session-test-"));
  return path.join(dir, "ssh-state.json");
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

describe("SessionPersistence", () => {
  let statePath: string;
  let sessionManager: MockSshSessionManager;
  let profileManager: MockSshProfileManager;
  let persistence: SessionPersistence;

  beforeEach(() => {
    statePath = createTempStatePath();
    sessionManager = new MockSshSessionManager();
    profileManager = new MockSshProfileManager();
    persistence = new SessionPersistence(
      sessionManager as any,
      profileManager as any,
      statePath,
    );
  });

  afterEach(() => {
    cleanupPath(statePath);
  });

  describe("saveState", () => {
    it("writes active connection profile IDs to the state file", () => {
      sessionManager.addConnection("gpu-server", "connected");
      sessionManager.addConnection("build-machine", "connected");

      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.version).toBe(1);
      expect(state.activeConnections).toContain("gpu-server");
      expect(state.activeConnections).toContain("build-machine");
      expect(state.activeConnections).toHaveLength(2);
      expect(state.savedAt).toBeGreaterThan(0);
    });

    it("includes reconnecting connections", () => {
      sessionManager.addConnection("gpu-server", "connected");
      sessionManager.addConnection("flaky-server", "reconnecting");

      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.activeConnections).toContain("gpu-server");
      expect(state.activeConnections).toContain("flaky-server");
      expect(state.activeConnections).toHaveLength(2);
    });

    it("excludes disconnected connections", () => {
      sessionManager.addConnection("gpu-server", "connected");
      sessionManager.addConnection("offline-server", "disconnected");

      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.activeConnections).toContain("gpu-server");
      expect(state.activeConnections).not.toContain("offline-server");
      expect(state.activeConnections).toHaveLength(1);
    });

    it("excludes connections in error state", () => {
      sessionManager.addConnection("gpu-server", "connected");
      sessionManager.addConnection("broken-server", "error");

      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.activeConnections).not.toContain("broken-server");
      expect(state.activeConnections).toHaveLength(1);
    });

    it("excludes connections in connecting state", () => {
      sessionManager.addConnection("gpu-server", "connected");
      sessionManager.addConnection("pending-server", "connecting");

      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.activeConnections).not.toContain("pending-server");
      expect(state.activeConnections).toHaveLength(1);
    });

    it("writes an empty array when no connections are active", () => {
      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.version).toBe(1);
      expect(state.activeConnections).toEqual([]);
      expect(state.savedAt).toBeGreaterThan(0);
    });

    it("creates parent directories if they do not exist", () => {
      const deepPath = path.join(
        os.tmpdir(),
        `crab-deep-${Date.now()}`,
        "nested",
        "dir",
        "ssh-state.json",
      );
      const deepPersistence = new SessionPersistence(
        sessionManager as any,
        profileManager as any,
        deepPath,
      );

      sessionManager.addConnection("gpu-server", "connected");
      deepPersistence.saveState();

      expect(fs.existsSync(deepPath)).toBe(true);

      // Cleanup
      const root = path.join(os.tmpdir(), path.basename(path.dirname(path.dirname(path.dirname(deepPath)))));
      fs.rmSync(path.dirname(path.dirname(path.dirname(deepPath))), { recursive: true, force: true });
    });

    it("overwrites existing state file", () => {
      sessionManager.addConnection("server-a", "connected");
      persistence.saveState();

      // Change connections
      sessionManager.addConnection("server-b", "connected");
      persistence.saveState();

      const content = fs.readFileSync(statePath, "utf-8");
      const state: SshSessionState = JSON.parse(content);

      expect(state.activeConnections).toContain("server-a");
      expect(state.activeConnections).toContain("server-b");
    });
  });

  describe("getProfileIdsToReconnect", () => {
    it("returns profile IDs that have autoReconnect enabled", () => {
      profileManager.addProfile("gpu-server", true);
      profileManager.addProfile("build-machine", true);

      const state: SshSessionState = {
        version: 1,
        activeConnections: ["gpu-server", "build-machine"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      const result = persistence.getProfileIdsToReconnect();

      expect(result).toContain("gpu-server");
      expect(result).toContain("build-machine");
      expect(result).toHaveLength(2);
    });

    it("excludes profiles with autoReconnect disabled", () => {
      profileManager.addProfile("gpu-server", true);
      profileManager.addProfile("manual-server", false);

      const state: SshSessionState = {
        version: 1,
        activeConnections: ["gpu-server", "manual-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      const result = persistence.getProfileIdsToReconnect();

      expect(result).toContain("gpu-server");
      expect(result).not.toContain("manual-server");
      expect(result).toHaveLength(1);
    });

    it("excludes profiles that no longer exist", () => {
      profileManager.addProfile("gpu-server", true);
      // "deleted-server" is NOT added to profileManager

      const state: SshSessionState = {
        version: 1,
        activeConnections: ["gpu-server", "deleted-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      const result = persistence.getProfileIdsToReconnect();

      expect(result).toContain("gpu-server");
      expect(result).not.toContain("deleted-server");
      expect(result).toHaveLength(1);
    });

    it("returns empty array when state file does not exist", () => {
      const result = persistence.getProfileIdsToReconnect();
      expect(result).toEqual([]);
    });

    it("returns empty array when state file is corrupted JSON", () => {
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, "not valid json {{{", "utf-8");

      const result = persistence.getProfileIdsToReconnect();
      expect(result).toEqual([]);
    });

    it("returns empty array when state file has wrong version", () => {
      const state = {
        version: 99,
        activeConnections: ["gpu-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      profileManager.addProfile("gpu-server", true);

      const result = persistence.getProfileIdsToReconnect();
      expect(result).toEqual([]);
    });

    it("returns empty array when activeConnections is not an array", () => {
      const state = {
        version: 1,
        activeConnections: "not-an-array",
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      const result = persistence.getProfileIdsToReconnect();
      expect(result).toEqual([]);
    });

    it("filters out non-string entries in activeConnections", () => {
      profileManager.addProfile("gpu-server", true);

      const state = {
        version: 1,
        activeConnections: ["gpu-server", 123, null, undefined, "gpu-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      const result = persistence.getProfileIdsToReconnect();

      // "gpu-server" appears twice in the array but both pass the filter
      expect(result).toEqual(["gpu-server", "gpu-server"]);
    });

    it("returns empty array when state file is empty", () => {
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, "", "utf-8");

      const result = persistence.getProfileIdsToReconnect();
      expect(result).toEqual([]);
    });
  });

  describe("clearState", () => {
    it("removes the state file", () => {
      sessionManager.addConnection("gpu-server", "connected");
      persistence.saveState();

      expect(fs.existsSync(statePath)).toBe(true);

      persistence.clearState();

      expect(fs.existsSync(statePath)).toBe(false);
    });

    it("does not throw when state file does not exist", () => {
      expect(() => persistence.clearState()).not.toThrow();
    });
  });

  describe("removeProfile", () => {
    it("removes a single profile from persisted state", () => {
      const state: SshSessionState = {
        version: 1,
        activeConnections: ["gpu-server", "build-machine", "dev-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      persistence.removeProfile("build-machine");

      const content = fs.readFileSync(statePath, "utf-8");
      const updated: SshSessionState = JSON.parse(content);

      expect(updated.activeConnections).toContain("gpu-server");
      expect(updated.activeConnections).toContain("dev-server");
      expect(updated.activeConnections).not.toContain("build-machine");
      expect(updated.activeConnections).toHaveLength(2);
    });

    it("clears state file when removing the last profile", () => {
      const state: SshSessionState = {
        version: 1,
        activeConnections: ["gpu-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      persistence.removeProfile("gpu-server");

      expect(fs.existsSync(statePath)).toBe(false);
    });

    it("does nothing when state file does not exist", () => {
      expect(() => persistence.removeProfile("nonexistent")).not.toThrow();
    });

    it("does nothing when profile is not in the state", () => {
      const state: SshSessionState = {
        version: 1,
        activeConnections: ["gpu-server"],
        savedAt: Date.now(),
      };
      fs.mkdirSync(path.dirname(statePath), { recursive: true });
      fs.writeFileSync(statePath, JSON.stringify(state), "utf-8");

      persistence.removeProfile("not-in-state");

      const content = fs.readFileSync(statePath, "utf-8");
      const updated: SshSessionState = JSON.parse(content);
      expect(updated.activeConnections).toEqual(["gpu-server"]);
    });
  });

  describe("round-trip: save then restore", () => {
    it("saves and restores active connections correctly", () => {
      profileManager.addProfile("gpu-server", true);
      profileManager.addProfile("build-machine", true);

      sessionManager.addConnection("gpu-server", "connected", true);
      sessionManager.addConnection("build-machine", "connected", true);

      persistence.saveState();

      // Create a new persistence instance (simulating app restart)
      const restored = new SessionPersistence(
        sessionManager as any,
        profileManager as any,
        statePath,
      );

      const toReconnect = restored.getProfileIdsToReconnect();

      expect(toReconnect).toContain("gpu-server");
      expect(toReconnect).toContain("build-machine");
      expect(toReconnect).toHaveLength(2);
    });

    it("respects autoReconnect changes between save and restore", () => {
      profileManager.addProfile("gpu-server", true);
      profileManager.addProfile("build-machine", true);

      sessionManager.addConnection("gpu-server", "connected", true);
      sessionManager.addConnection("build-machine", "connected", true);

      persistence.saveState();

      // Simulate user disabling autoReconnect on build-machine
      const updatedProfileManager = new MockSshProfileManager();
      updatedProfileManager.addProfile("gpu-server", true);
      updatedProfileManager.addProfile("build-machine", false); // Changed!

      const restored = new SessionPersistence(
        sessionManager as any,
        updatedProfileManager as any,
        statePath,
      );

      const toReconnect = restored.getProfileIdsToReconnect();

      expect(toReconnect).toContain("gpu-server");
      expect(toReconnect).not.toContain("build-machine");
      expect(toReconnect).toHaveLength(1);
    });
  });
});
