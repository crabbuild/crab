/**
 * Unit tests for the RemoteDebugManager.
 *
 * Mocks RpcRouter, SshSessionManager, and net.Server to test:
 * - Debug session lifecycle (start, stop)
 * - Port forwarding setup with timeout
 * - Alternative port selection when port is in use
 * - Debugger URL generation for each runtime
 * - Cleanup on session end
 * - Timeout and unexpected termination handling
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { EventEmitter } from "node:events";

// ─── Mocks ───────────────────────────────────────────────────────────────────

let listenBehavior: "success" | "eaddrinuse" | "eaddrinuse-then-success" = "success";
let eaddrinuseCallCount = 0;

class MockServer extends EventEmitter {
  listening = false;
  address = vi.fn(() => ({ port: 9229, address: "127.0.0.1" }));
  close = vi.fn((cb?: () => void) => {
    this.listening = false;
    if (cb) cb();
  });
  removeAllListeners = vi.fn(function (this: MockServer, _event?: string) {
    return this;
  });
  listen = vi.fn((_port: number, _host: string, cb: () => void) => {
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
}

const mockServerInstances: MockServer[] = [];

vi.mock("node:net", () => ({
  createServer: vi.fn((_handler: unknown) => {
    const server = new MockServer();
    mockServerInstances.push(server);
    return server;
  }),
}));

class MockClient extends EventEmitter {
  forwardOut = vi.fn(
    (
      _srcAddr: string,
      _srcPort: number,
      _dstAddr: string,
      _dstPort: number,
      cb: (err: Error | null, channel: unknown) => void,
    ) => {
      const channel = new EventEmitter();
      (channel as any).pipe = vi.fn(() => channel);
      (channel as any).close = vi.fn();
      cb(null, channel);
    },
  );
}

function createMockSessionManager(status = "connected") {
  const mockClient = new MockClient();
  const manager = new EventEmitter() as EventEmitter & {
    getConnection: ReturnType<typeof vi.fn>;
    getStatus: ReturnType<typeof vi.fn>;
  };
  manager.getConnection = vi.fn((profileId: string) => {
    if (profileId === "test-profile") {
      return {
        profile: { id: "test-profile", host: "test.example.com" },
        client: mockClient,
        status,
        channels: { terminals: new Map(), portForwards: new Map() },
        jumpClients: [],
      };
    }
    return undefined;
  });
  manager.getStatus = vi.fn((_profileId: string) => status);
  return { manager, mockClient };
}

function createMockRouter() {
  return {
    sendToRemoteAgent: vi.fn(
      async (_profileId: string, method: string, params: any) => {
        if (method === "port_check") {
          return { available: true };
        }
        if (method === "debug_launch") {
          return {
            pid: 12345,
            port: params.port ?? 9229,
            protocol: "chrome-devtools",
          };
        }
        if (method === "debug_stop") {
          return { success: true };
        }
        return {};
      },
    ),
  };
}

// ─── Import under test ───────────────────────────────────────────────────────

import { RemoteDebugManager } from "../electron/ssh-remote-debug";
import type { RpcRouter } from "../electron/ssh-router";
import type { SshSessionManager } from "../electron/ssh-manager";

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("RemoteDebugManager", () => {
  let debugManager: RemoteDebugManager;
  let mockRouter: ReturnType<typeof createMockRouter>;
  let sessionManager: ReturnType<typeof createMockSessionManager>;

  beforeEach(() => {
    mockServerInstances.length = 0;
    listenBehavior = "success";
    eaddrinuseCallCount = 0;
    mockRouter = createMockRouter();
    sessionManager = createMockSessionManager();
    debugManager = new RemoteDebugManager(
      mockRouter as unknown as RpcRouter,
      sessionManager.manager as unknown as SshSessionManager,
    );
  });

  afterEach(async () => {
    await debugManager.dispose();
  });

  describe("startSession", () => {
    it("launches a debug process and sets up port forwarding", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(session.id).toBeDefined();
      expect(session.profileId).toBe("test-profile");
      expect(session.runtime).toBe("node");
      expect(session.protocol).toBe("chrome-devtools");
      expect(session.remotePid).toBe(12345);
      expect(session.status).toBe("active");
      expect(session.localPort).toBe(9229);
    });

    it("calls port_check RPC before launching", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "test-profile",
        "port_check",
        { port: 9229 },
      );
    });

    it("calls debug_launch RPC with correct params", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node --inspect=0.0.0.0:9229 app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "test-profile",
        "debug_launch",
        expect.objectContaining({
          runtime: "node",
          cwd: "/remote/workspace",
          port: 9229,
        }),
      );
    });

    it("generates Chrome DevTools URL for Node.js", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(session.debugUrl).toContain("devtools://");
      expect(session.debugUrl).toContain("9229");
    });

    it("generates debugpy URL for Python", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "python",
        command: "python main.py",
        cwd: "/remote/workspace",
        port: 5678,
      });

      expect(session.protocol).toBe("debugpy");
      expect(session.debugUrl).toContain("debugpy://");
    });

    it("generates gdbserver URL for Rust/C++", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "rust",
        command: "./target/debug/myapp",
        cwd: "/remote/workspace",
        port: 1234,
      });

      expect(session.protocol).toBe("gdbserver");
      expect(session.debugUrl).toContain("gdb://");
    });

    it("throws when SSH connection is not active", async () => {
      const disconnected = createMockSessionManager("disconnected");
      const mgr = new RemoteDebugManager(
        mockRouter as unknown as RpcRouter,
        disconnected.manager as unknown as SshSessionManager,
      );

      await expect(
        mgr.startSession("test-profile", {
          runtime: "node",
          command: "node app.js",
          cwd: "/remote/workspace",
          port: 9229,
        }),
      ).rejects.toThrow("disconnected");

      await mgr.dispose();
    });

    it("emits debug-session-started event on success", async () => {
      const events: unknown[] = [];
      debugManager.on("debug-session-started", (s) => events.push(s));

      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(events).toHaveLength(1);
      expect((events[0] as any).status).toBe("active");
    });
  });

  describe("alternative port selection", () => {
    it("selects alternative port when requested port is in use", async () => {
      let portCheckCount = 0;
      mockRouter.sendToRemoteAgent.mockImplementation(
        async (_profileId: string, method: string, params: any) => {
          if (method === "port_check") {
            portCheckCount++;
            // First port unavailable, second available
            if (params.port === 9229) {
              return { available: false, alternative: 9230 };
            }
            return { available: true };
          }
          if (method === "debug_launch") {
            return { pid: 12345, port: params.port, protocol: "chrome-devtools" };
          }
          return {};
        },
      );

      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(session.remotePort).toBe(9230);
      expect(portCheckCount).toBeGreaterThan(1);
    });

    it("uses alternative local port when local port is in use", async () => {
      listenBehavior = "eaddrinuse-then-success";

      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      expect(session.localPort).toBe(9230);
    });

    it("throws when no ports are available", async () => {
      mockRouter.sendToRemoteAgent.mockImplementation(
        async (_profileId: string, method: string, _params: any) => {
          if (method === "port_check") {
            return { available: false };
          }
          return {};
        },
      );

      await expect(
        debugManager.startSession("test-profile", {
          runtime: "node",
          command: "node app.js",
          cwd: "/remote/workspace",
          port: 9229,
        }),
      ).rejects.toThrow("No available debug port");
    });
  });

  describe("stopSession", () => {
    it("calls debug_stop RPC and cleans up", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      await debugManager.stopSession(session.id);

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "test-profile",
        "debug_stop",
        { pid: 12345 },
      );
    });

    it("closes the port forward server", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      await debugManager.stopSession(session.id);

      expect(mockServerInstances[0].close).toHaveBeenCalled();
    });

    it("emits debug-session-stopped event", async () => {
      const events: unknown[] = [];
      debugManager.on("debug-session-stopped", (payload) => events.push(payload));

      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      await debugManager.stopSession(session.id);

      expect(events).toHaveLength(1);
      expect((events[0] as any).sessionId).toBe(session.id);
    });

    it("is idempotent for non-existent sessions", async () => {
      await expect(debugManager.stopSession("nonexistent")).resolves.toBeUndefined();
    });

    it("handles agent unreachable gracefully during stop", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      // Make the router throw on debug_stop
      mockRouter.sendToRemoteAgent.mockImplementation(
        async (_profileId: string, method: string) => {
          if (method === "debug_stop") {
            throw new Error("Agent unreachable");
          }
          return {};
        },
      );

      // Should not throw
      await expect(debugManager.stopSession(session.id)).resolves.toBeUndefined();
    });
  });

  describe("timeout handling", () => {
    it("aborts session if setup exceeds 15 seconds", async () => {
      // Make debug_launch hang
      mockRouter.sendToRemoteAgent.mockImplementation(
        async (_profileId: string, method: string, params: any) => {
          if (method === "port_check") {
            return { available: true };
          }
          if (method === "debug_launch") {
            // Never resolves within timeout
            return new Promise(() => {});
          }
          return {};
        },
      );

      vi.useFakeTimers();

      const promise = debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      // Advance past the 15s timeout
      vi.advanceTimersByTime(16_000);

      await expect(promise).rejects.toThrow("timed out");

      vi.useRealTimers();
    });

    it("emits error event on timeout", async () => {
      const errors: unknown[] = [];
      debugManager.on("debug-session-error", (payload) => errors.push(payload));

      mockRouter.sendToRemoteAgent.mockImplementation(
        async (_profileId: string, method: string) => {
          if (method === "port_check") {
            return { available: true };
          }
          if (method === "debug_launch") {
            return new Promise(() => {});
          }
          return {};
        },
      );

      vi.useFakeTimers();

      const promise = debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      vi.advanceTimersByTime(16_000);

      await expect(promise).rejects.toThrow();
      expect(errors).toHaveLength(1);
      expect((errors[0] as any).error).toContain("timed out");

      vi.useRealTimers();
    });
  });

  describe("unexpected termination", () => {
    it("emits error when SSH connection drops during active session", async () => {
      const errors: unknown[] = [];
      debugManager.on("debug-session-error", (payload) => errors.push(payload));

      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      // Simulate SSH disconnection
      sessionManager.manager.emit("ssh:status-changed", {
        profileId: "test-profile",
        status: "disconnected",
      });

      expect(errors).toHaveLength(1);
      expect((errors[0] as any).sessionId).toBe(session.id);
      expect((errors[0] as any).error).toContain("connection lost");
    });

    it("transitions session to error state on disconnect", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      sessionManager.manager.emit("ssh:status-changed", {
        profileId: "test-profile",
        status: "disconnected",
      });

      const updated = debugManager.getSession(session.id);
      expect(updated?.status).toBe("error");
    });
  });

  describe("session management", () => {
    it("getSession returns the session by ID", async () => {
      const session = await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      const retrieved = debugManager.getSession(session.id);
      expect(retrieved).toEqual(session);
    });

    it("listSessions returns all sessions for a profile", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      const sessions = debugManager.listSessions("test-profile");
      expect(sessions).toHaveLength(1);
    });

    it("stopAllForProfile stops all active sessions", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      await debugManager.stopAllForProfile("test-profile");

      const sessions = debugManager.listSessions("test-profile");
      expect(sessions.every((s) => s.status === "terminated")).toBe(true);
    });
  });

  describe("debug command building", () => {
    it("injects --inspect flag for Node.js when not present", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      const launchCall = mockRouter.sendToRemoteAgent.mock.calls.find(
        (c) => c[1] === "debug_launch",
      );
      expect(launchCall?.[2].command).toContain("--inspect=0.0.0.0:9229");
    });

    it("preserves existing --inspect flag for Node.js", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "node",
        command: "node --inspect=0.0.0.0:9229 app.js",
        cwd: "/remote/workspace",
        port: 9229,
      });

      const launchCall = mockRouter.sendToRemoteAgent.mock.calls.find(
        (c) => c[1] === "debug_launch",
      );
      expect(launchCall?.[2].command).toBe("node --inspect=0.0.0.0:9229 app.js");
    });

    it("wraps Python command with debugpy", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "python",
        command: "python main.py",
        cwd: "/remote/workspace",
        port: 5678,
      });

      const launchCall = mockRouter.sendToRemoteAgent.mock.calls.find(
        (c) => c[1] === "debug_launch",
      );
      expect(launchCall?.[2].command).toContain("debugpy");
      expect(launchCall?.[2].command).toContain("--listen 0.0.0.0:5678");
    });

    it("wraps Rust/C++ command with gdbserver", async () => {
      await debugManager.startSession("test-profile", {
        runtime: "rust",
        command: "./target/debug/myapp",
        cwd: "/remote/workspace",
        port: 1234,
      });

      const launchCall = mockRouter.sendToRemoteAgent.mock.calls.find(
        (c) => c[1] === "debug_launch",
      );
      expect(launchCall?.[2].command).toContain("gdbserver 0.0.0.0:1234");
    });
  });
});
