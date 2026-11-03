/**
 * Unit tests for the AgentDetector class.
 *
 * Tests detection of remote agent CLIs (claude, codex) via both
 * RPC router and SSH exec channel fallback paths.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock electron (not used directly but imported transitively)
vi.mock("electron", () => ({
  powerMonitor: new EventEmitter(),
}));

// Mock keychain
vi.mock("../electron/keychain", () => ({
  getPassword: vi.fn(async () => null),
}));

// Mock fs
vi.mock("node:fs", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:fs")>();
  return {
    ...actual,
    existsSync: vi.fn(() => true),
    readFileSync: vi.fn(() => Buffer.from("mock")),
  };
});

// Mock ssh2
class MockClientChannel extends EventEmitter {
  end = vi.fn();
  write = vi.fn();
  close = vi.fn();
  stderr = new EventEmitter();
}

class MockClient extends EventEmitter {
  connect = vi.fn();
  end = vi.fn();
  exec = vi.fn();
}

vi.mock("ssh2", () => ({
  Client: vi.fn(() => new MockClient()),
}));

import { AgentDetector } from "../electron/ssh-agent-detection";
import type { AgentDetectionResult } from "../electron/ssh-agent-detection";
import type { SshSessionManager, SshConnection } from "../electron/ssh-manager";
import type { RpcRouter } from "../electron/ssh-router";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createMockRouter(responses: Record<string, unknown>): RpcRouter {
  return {
    route: vi.fn(async (_repoId: string, method: string, params: unknown) => {
      const command = (params as { command?: string }).command;
      const key = command ?? method;
      if (key in responses) {
        return responses[key];
      }
      throw new Error(`Unhandled RPC: ${method} ${command ?? ""}`);
    }),
  } as unknown as RpcRouter;
}

function createMockSessionManager(conn?: Partial<SshConnection>): SshSessionManager {
  const manager = new EventEmitter() as unknown as SshSessionManager;
  (manager as any).getConnection = vi.fn(() => conn ?? undefined);
  (manager as any).getStatus = vi.fn(() => conn?.status ?? "disconnected");
  return manager;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("AgentDetector", () => {
  let detector: AgentDetector;
  let mockSessionManager: SshSessionManager;

  beforeEach(() => {
    mockSessionManager = createMockSessionManager();
  });

  describe("detect via RPC router", () => {
    it("detects both claude and codex when available", async () => {
      const router = createMockRouter({
        detect_agents: {
          agents: [
            { name: "claude", path: "/usr/local/bin/claude", available: true, version: "1.2.0" },
            { name: "codex", path: "/usr/local/bin/codex", available: true, version: "0.5.1" },
          ],
        },
      });

      detector = new AgentDetector(mockSessionManager, router);
      const result = await detector.detect("profile-1");

      expect(result.profileId).toBe("profile-1");
      expect(result.agents).toHaveLength(2);
      expect(result.agents[0]).toEqual({
        name: "claude",
        path: "/usr/local/bin/claude",
        available: true,
        version: "1.2.0",
      });
      expect(result.agents[1]).toEqual({
        name: "codex",
        path: "/usr/local/bin/codex",
        available: true,
        version: "0.5.1",
      });
      expect(result.detectedAt).toBeGreaterThan(0);
    });

    it("falls back to exec_command when detect_agents RPC is not supported", async () => {
      const router = createMockRouter({
        "which claude": { stdout: "/home/user/.local/bin/claude\n", exitCode: 0 },
        "which codex": { stdout: "", exitCode: 1 },
        "claude --version 2>/dev/null || true": { stdout: "claude 1.0.3\n", exitCode: 0 },
      });

      detector = new AgentDetector(mockSessionManager, router);
      const result = await detector.detect("profile-2");

      expect(result.agents).toHaveLength(2);
      expect(result.agents[0]).toEqual({
        name: "claude",
        path: "/home/user/.local/bin/claude",
        available: true,
        version: "claude 1.0.3",
      });
      expect(result.agents[1]).toEqual({
        name: "codex",
        path: "",
        available: false,
        version: undefined,
      });
    });

    it("handles RPC errors gracefully", async () => {
      const router = {
        route: vi.fn(async () => {
          throw new Error("Connection lost");
        }),
      } as unknown as RpcRouter;

      detector = new AgentDetector(mockSessionManager, router);
      const result = await detector.detect("profile-3");

      // All agents should be marked unavailable
      expect(result.agents).toHaveLength(2);
      expect(result.agents.every((a) => !a.available)).toBe(true);
    });

    it("handles partial failures (one CLI found, one errors)", async () => {
      let callCount = 0;
      const router = {
        route: vi.fn(async (_repoId: string, method: string, params: unknown) => {
          if (method === "detect_agents") throw new Error("not supported");
          const command = (params as { command?: string }).command;
          callCount++;
          if (command === "which claude") {
            return { stdout: "/usr/bin/claude\n", exitCode: 0 };
          }
          if (command === "which codex") {
            throw new Error("timeout");
          }
          if (command === "claude --version 2>/dev/null || true") {
            return { stdout: "", exitCode: 1 };
          }
          throw new Error("unexpected");
        }),
      } as unknown as RpcRouter;

      detector = new AgentDetector(mockSessionManager, router);
      const result = await detector.detect("profile-4");

      expect(result.agents[0]).toEqual({
        name: "claude",
        path: "/usr/bin/claude",
        available: true,
        version: undefined,
      });
      expect(result.agents[1]).toEqual({
        name: "codex",
        path: "",
        available: false,
        version: undefined,
      });
    });
  });

  describe("detect via SSH exec channel (no router)", () => {
    it("detects agents using SSH exec when no router is available", async () => {
      const mockClient = new MockClient();
      mockClient.exec = vi.fn((command: string, callback: Function) => {
        const channel = new MockClientChannel();
        callback(null, channel);

        // Simulate async response
        setTimeout(() => {
          if (command === "which claude") {
            channel.emit("data", Buffer.from("/usr/local/bin/claude\n"));
            channel.emit("close", 0);
          } else if (command === "which codex") {
            channel.emit("data", Buffer.from(""));
            channel.emit("close", 1);
          } else if (command.includes("--version")) {
            channel.emit("data", Buffer.from("claude v2.1.0\n"));
            channel.emit("close", 0);
          }
        }, 10);
      });

      const conn: Partial<SshConnection> = {
        client: mockClient as any,
        status: "connected",
      };

      mockSessionManager = createMockSessionManager(conn);
      detector = new AgentDetector(mockSessionManager);
      const result = await detector.detect("profile-5");

      expect(result.agents[0]).toEqual({
        name: "claude",
        path: "/usr/local/bin/claude",
        available: true,
        version: "claude v2.1.0",
      });
      expect(result.agents[1]).toEqual({
        name: "codex",
        path: "",
        available: false,
        version: undefined,
      });
    });

    it("returns all unavailable when connection is not active", async () => {
      mockSessionManager = createMockSessionManager(undefined);
      detector = new AgentDetector(mockSessionManager);
      const result = await detector.detect("profile-6");

      expect(result.agents).toHaveLength(2);
      expect(result.agents.every((a) => !a.available)).toBe(true);
    });
  });

  describe("caching", () => {
    it("caches detection results", async () => {
      const router = createMockRouter({
        detect_agents: {
          agents: [
            { name: "claude", path: "/usr/bin/claude", available: true },
            { name: "codex", path: "", available: false },
          ],
        },
      });

      detector = new AgentDetector(mockSessionManager, router);
      await detector.detect("profile-7");

      const cached = detector.getCached("profile-7");
      expect(cached).toBeDefined();
      expect(cached!.agents[0].available).toBe(true);
    });

    it("returns undefined for uncached profiles", () => {
      detector = new AgentDetector(mockSessionManager);
      expect(detector.getCached("nonexistent")).toBeUndefined();
    });

    it("clears cache on request", async () => {
      const router = createMockRouter({
        detect_agents: {
          agents: [
            { name: "claude", path: "/usr/bin/claude", available: true },
            { name: "codex", path: "", available: false },
          ],
        },
      });

      detector = new AgentDetector(mockSessionManager, router);
      await detector.detect("profile-8");
      expect(detector.getCached("profile-8")).toBeDefined();

      detector.clearCache("profile-8");
      expect(detector.getCached("profile-8")).toBeUndefined();
    });
  });

  describe("events", () => {
    it("emits ssh:agents-detected event on successful detection", async () => {
      const router = createMockRouter({
        detect_agents: {
          agents: [
            { name: "claude", path: "/usr/bin/claude", available: true },
            { name: "codex", path: "/usr/bin/codex", available: true },
          ],
        },
      });

      detector = new AgentDetector(mockSessionManager, router);

      const emitted: AgentDetectionResult[] = [];
      detector.on("ssh:agents-detected", (result) => {
        emitted.push(result);
      });

      await detector.detect("profile-9");

      expect(emitted).toHaveLength(1);
      expect(emitted[0].profileId).toBe("profile-9");
      expect(emitted[0].agents).toHaveLength(2);
    });
  });

  describe("setRouter", () => {
    it("allows setting the router after construction", async () => {
      detector = new AgentDetector(mockSessionManager);

      const router = createMockRouter({
        detect_agents: {
          agents: [
            { name: "claude", path: "/usr/bin/claude", available: true },
            { name: "codex", path: "", available: false },
          ],
        },
      });

      detector.setRouter(router);
      const result = await detector.detect("profile-10");

      expect(result.agents[0].available).toBe(true);
    });
  });
});
