/**
 * Unit tests for the PortForwardManager.
 *
 * Mocks net.Server, ssh2 Client, and SshSessionManager to test:
 * - Forward lifecycle (add, list, remove)
 * - Auto-start on connection
 * - Auto-restart on reconnection
 * - Port conflict handling (EADDRINUSE retry)
 * - Dynamic detection from terminal output
 * - Open in browser action
 */

import { describe, it, expect, beforeEach, vi, type Mock } from "vitest";
import { EventEmitter } from "node:events";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock electron shell
vi.mock("electron", () => ({
  shell: {
    openExternal: vi.fn().mockResolvedValue(undefined),
  },
}));

// Configurable listen behavior for the mock server
let listenBehavior: "success" | "eaddrinuse" | "eaddrinuse-then-success" = "success";
let eaddrinuseCallCount = 0;

class MockServer extends EventEmitter {
  listening = false;
  address = vi.fn(() => ({ port: 8888, address: "127.0.0.1" }));
  close = vi.fn((cb?: () => void) => {
    this.listening = false;
    if (cb) cb();
  });
  removeAllListeners = vi.fn(function (this: MockServer, _event?: string) {
    return this;
  });
  listen = vi.fn((port: number, _host: string, cb: () => void) => {
    if (listenBehavior === "success") {
      this.listening = true;
      process.nextTick(() => cb());
    } else if (listenBehavior === "eaddrinuse") {
      const err = new Error("listen EADDRINUSE") as NodeJS.ErrnoException;
      err.code = "EADDRINUSE";
      process.nextTick(() => this.emit("error", err));
    } else if (listenBehavior === "eaddrinuse-then-success") {
      eaddrinuseCallCount++;
      if (eaddrinuseCallCount === 1) {
        const err = new Error("listen EADDRINUSE") as NodeJS.ErrnoException;
        err.code = "EADDRINUSE";
        process.nextTick(() => this.emit("error", err));
      } else {
        this.listening = true;
        process.nextTick(() => cb());
      }
    }
  });
  connectionHandler: ((socket: MockSocket) => void) | null = null;
}

class MockSocket extends EventEmitter {
  destroyed = false;
  pipe = vi.fn(() => this);
  unpipe = vi.fn();
  destroy = vi.fn(() => {
    this.destroyed = true;
  });
  write = vi.fn();
}

class MockChannel extends EventEmitter {
  destroyed = false;
  pipe = vi.fn(() => this);
  unpipe = vi.fn();
  close = vi.fn(() => {
    this.destroyed = true;
  });
  write = vi.fn();
}

const mockServerInstances: MockServer[] = [];

vi.mock("node:net", () => ({
  createServer: vi.fn((handler: (socket: MockSocket) => void) => {
    const server = new MockServer();
    server.connectionHandler = handler;
    mockServerInstances.push(server);
    return server;
  }),
}));

class MockClient extends EventEmitter {
  forwardOut = vi.fn();
}

function createMockSessionManager(client?: MockClient, status: string = "connected") {
  const mockClient = client ?? new MockClient();
  return {
    getConnection: vi.fn((profileId: string) => {
      if (profileId === "test-profile") {
        return {
          profile: {
            id: "test-profile",
            host: "test.example.com",
            portForwards: [],
          },
          client: mockClient,
          status,
          channels: { terminals: new Map(), portForwards: new Map() },
          jumpClients: [],
        };
      }
      return undefined;
    }),
    mockClient,
  };
}

// ─── Import under test ───────────────────────────────────────────────────────

import { PortForwardManager } from "../electron/ssh-port-forward";
import type { SshSessionManager } from "../electron/ssh-manager";

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("PortForwardManager", () => {
  let manager: PortForwardManager;
  let mockClient: MockClient;
  let sessionManager: ReturnType<typeof createMockSessionManager>;

  beforeEach(() => {
    mockServerInstances.length = 0;
    listenBehavior = "success";
    eaddrinuseCallCount = 0;
    mockClient = new MockClient();
    sessionManager = createMockSessionManager(mockClient);
    manager = new PortForwardManager(
      sessionManager as unknown as SshSessionManager,
    );
  });

  describe("addLocalForward", () => {
    it("creates a forward and returns an ID", async () => {
      const id = await manager.addLocalForward(
        "test-profile",
        8888,
        "127.0.0.1",
        8888,
        "Jupyter",
      );

      expect(id).toBeDefined();
      expect(typeof id).toBe("string");
      expect(id.length).toBeGreaterThan(0);
    });

    it("starts a TCP server on the specified port", async () => {
      await manager.addLocalForward("test-profile", 9000, "127.0.0.1", 9000);

      expect(mockServerInstances).toHaveLength(1);
      expect(mockServerInstances[0].listen).toHaveBeenCalledWith(
        9000,
        "127.0.0.1",
        expect.any(Function),
      );
    });

    it("throws when profile is not connected", async () => {
      const disconnectedManager = createMockSessionManager(mockClient, "disconnected");
      const mgr = new PortForwardManager(
        disconnectedManager as unknown as SshSessionManager,
      );

      await expect(
        mgr.addLocalForward("test-profile", 8888, "127.0.0.1", 8888),
      ).rejects.toThrow("not active");
    });

    it("throws when profile does not exist", async () => {
      await expect(
        manager.addLocalForward("nonexistent", 8888, "127.0.0.1", 8888),
      ).rejects.toThrow("not active");
    });

    it("emits status event on success", async () => {
      const statusEvents: unknown[] = [];
      manager.on("ssh:port-forward-status", (payload) => statusEvents.push(payload));

      await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      expect(statusEvents).toHaveLength(1);
      expect(statusEvents[0]).toMatchObject({
        status: "active",
        actualLocalPort: 8888,
      });
    });

    it("stores label and autoStart", async () => {
      const id = await manager.addLocalForward(
        "test-profile",
        8888,
        "127.0.0.1",
        8888,
        "TensorBoard",
        true,
      );

      const forwards = manager.listForwards("test-profile");
      const fwd = forwards.find((f) => f.id === id);
      expect(fwd?.label).toBe("TensorBoard");
      expect(fwd?.autoStart).toBe(true);
    });
  });

  describe("port conflict handling", () => {
    it("retries on EADDRINUSE and uses next available port", async () => {
      listenBehavior = "eaddrinuse-then-success";

      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      const forwards = manager.listForwards();
      const fwd = forwards.find((f) => f.id === id);
      expect(fwd?.actualLocalPort).toBe(8889);
    });

    it("throws after MAX_PORT_RETRIES exhausted", async () => {
      listenBehavior = "eaddrinuse";

      await expect(
        manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888),
      ).rejects.toThrow("All ports");
    });
  });

  describe("removeForward", () => {
    it("closes the TCP server", async () => {
      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      manager.removeForward(id);

      expect(mockServerInstances[0].close).toHaveBeenCalled();
    });

    it("removes the forward from the list", async () => {
      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      manager.removeForward(id);

      const forwards = manager.listForwards();
      expect(forwards).toHaveLength(0);
    });

    it("is idempotent (no error on double remove)", async () => {
      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      manager.removeForward(id);
      manager.removeForward(id); // Should not throw
    });
  });

  describe("listForwards", () => {
    it("returns all forwards", async () => {
      await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888, "Jupyter");
      await manager.addLocalForward("test-profile", 6006, "127.0.0.1", 6006, "TensorBoard");

      const forwards = manager.listForwards();
      expect(forwards).toHaveLength(2);
    });

    it("filters by profileId", async () => {
      await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      const forwards = manager.listForwards("test-profile");
      expect(forwards).toHaveLength(1);

      const noForwards = manager.listForwards("other-profile");
      expect(noForwards).toHaveLength(0);
    });

    it("returns correct forward info shape", async () => {
      const id = await manager.addLocalForward(
        "test-profile",
        8888,
        "127.0.0.1",
        9999,
        "Jupyter",
        true,
      );

      const forwards = manager.listForwards();
      expect(forwards[0]).toMatchObject({
        id,
        profileId: "test-profile",
        localPort: 8888,
        actualLocalPort: 8888,
        remoteHost: "127.0.0.1",
        remotePort: 9999,
        label: "Jupyter",
        autoStart: true,
        status: "active",
      });
    });
  });

  describe("autoStartForwards", () => {
    it("starts forwards marked autoStart", async () => {
      const portForwards = [
        {
          id: "pf-1",
          profileId: "test-profile",
          direction: "local" as const,
          localPort: 8888,
          remoteHost: "127.0.0.1",
          remotePort: 8888,
          label: "Jupyter",
          autoStart: true,
          status: "stopped" as const,
        },
        {
          id: "pf-2",
          profileId: "test-profile",
          direction: "local" as const,
          localPort: 6006,
          remoteHost: "127.0.0.1",
          remotePort: 6006,
          label: "TensorBoard",
          autoStart: false,
          status: "stopped" as const,
        },
      ];

      await manager.autoStartForwards("test-profile", portForwards);

      const forwards = manager.listForwards();
      expect(forwards).toHaveLength(1);
      expect(forwards[0].label).toBe("Jupyter");
    });

    it("does not throw on individual forward failure", async () => {
      // Make the session manager return disconnected for the second call
      let callCount = 0;
      sessionManager.getConnection.mockImplementation((profileId: string) => {
        callCount++;
        if (callCount > 1) {
          return undefined; // Simulate failure on second forward
        }
        return {
          profile: { id: profileId, host: "test.example.com", portForwards: [] },
          client: mockClient,
          status: "connected",
          channels: { terminals: new Map(), portForwards: new Map() },
          jumpClients: [],
        };
      });

      const portForwards = [
        {
          id: "pf-1",
          profileId: "test-profile",
          direction: "local" as const,
          localPort: 8888,
          remoteHost: "127.0.0.1",
          remotePort: 8888,
          autoStart: true,
          status: "stopped" as const,
        },
        {
          id: "pf-2",
          profileId: "test-profile",
          direction: "local" as const,
          localPort: 6006,
          remoteHost: "127.0.0.1",
          remotePort: 6006,
          autoStart: true,
          status: "stopped" as const,
        },
      ];

      // Should not throw even though second forward will fail
      await expect(
        manager.autoStartForwards("test-profile", portForwards),
      ).resolves.toBeUndefined();
    });
  });

  describe("restoreForwards", () => {
    it("re-establishes previously active forwards", async () => {
      // Add a forward (which becomes active)
      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      // Simulate disconnect: stop the forward
      manager.stopAllForProfile("test-profile");

      const forwardsBefore = manager.listForwards();
      expect(forwardsBefore[0].status).toBe("stopped");

      // Restore on reconnection
      await manager.restoreForwards("test-profile");

      const forwardsAfter = manager.listForwards();
      expect(forwardsAfter[0].status).toBe("active");
      expect(forwardsAfter[0].id).toBe(id);
    });

    it("does nothing when no previously active forwards", async () => {
      await manager.restoreForwards("test-profile");

      const forwards = manager.listForwards();
      expect(forwards).toHaveLength(0);
    });
  });

  describe("detectFromTerminal", () => {
    it("detects Jupyter URL", () => {
      const result = manager.detectFromTerminal(
        "    Running on http://localhost:8888/lab",
      );

      expect(result).toMatchObject({
        remotePort: 8888,
        label: "Jupyter",
        remoteHost: "127.0.0.1",
      });
    });

    it("detects TensorBoard URL", () => {
      const result = manager.detectFromTerminal(
        "TensorBoard 2.14.0 at http://localhost:6006",
      );

      expect(result).toMatchObject({
        remotePort: 6006,
        label: "TensorBoard",
      });
    });

    it("detects generic listening port", () => {
      const result = manager.detectFromTerminal("Listening on port 3000");

      expect(result).toMatchObject({
        remotePort: 3000,
        label: "Server",
      });
    });

    it("detects FastAPI/Uvicorn URL", () => {
      const result = manager.detectFromTerminal(
        "Uvicorn running on http://0.0.0.0:8000",
      );

      expect(result).toMatchObject({
        remotePort: 8000,
        label: "FastAPI",
      });
    });

    it("detects Vite dev server", () => {
      const result = manager.detectFromTerminal(
        "  Local:   http://localhost:5173/",
      );

      expect(result).toMatchObject({
        remotePort: 5173,
        label: "Vite",
      });
    });

    it("returns null for non-matching output", () => {
      const result = manager.detectFromTerminal("just some random text");
      expect(result).toBeNull();
    });

    it("returns null for invalid port numbers", () => {
      const result = manager.detectFromTerminal(
        "Running on http://localhost:99999",
      );
      expect(result).toBeNull();
    });
  });

  describe("openInBrowser", () => {
    it("opens the correct URL for an active forward", async () => {
      const { shell } = await import("electron");
      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);

      await manager.openInBrowser(id);

      expect(shell.openExternal).toHaveBeenCalledWith("http://localhost:8888");
    });

    it("throws for non-existent forward", async () => {
      await expect(manager.openInBrowser("nonexistent")).rejects.toThrow(
        "not found",
      );
    });

    it("throws for non-active forward", async () => {
      const id = await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);
      manager.stopAllForProfile("test-profile");

      await expect(manager.openInBrowser(id)).rejects.toThrow("not active");
    });
  });

  describe("stopAllForProfile", () => {
    it("stops all active forwards for a profile", async () => {
      await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);
      await manager.addLocalForward("test-profile", 6006, "127.0.0.1", 6006);

      manager.stopAllForProfile("test-profile");

      const forwards = manager.listForwards();
      expect(forwards.every((f) => f.status === "stopped")).toBe(true);
    });

    it("closes all TCP servers", async () => {
      await manager.addLocalForward("test-profile", 8888, "127.0.0.1", 8888);
      await manager.addLocalForward("test-profile", 6006, "127.0.0.1", 6006);

      manager.stopAllForProfile("test-profile");

      for (const server of mockServerInstances) {
        expect(server.close).toHaveBeenCalled();
      }
    });
  });
});
