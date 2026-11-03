/**
 * Integration tests for the RemoteTerminalManager.
 *
 * Mocks the ssh2 Client and SshSessionManager to test:
 * - Opening a remote terminal (PTY allocation)
 * - Writing data to the terminal
 * - Receiving data from the terminal
 * - Resizing the terminal
 * - Closing the terminal
 * - Session persistence and reconnection recovery
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";

// ─── Mocks ───────────────────────────────────────────────────────────────────

class MockClientChannel extends EventEmitter {
  end = vi.fn(() => {
    // Simulate channel close on end
    setTimeout(() => this.emit("close"), 0);
  });
  write = vi.fn();
  setWindow = vi.fn();
  stderr = new EventEmitter();
}

class MockClient extends EventEmitter {
  shell = vi.fn();
}

function createMockSessionManager(client?: MockClient) {
  const mockClient = client ?? new MockClient();
  const manager = {
    getConnection: vi.fn((profileId: string) => {
      if (profileId === "connected-profile") {
        return {
          profile: { id: "connected-profile", host: "test.example.com" },
          client: mockClient,
          status: "connected" as const,
          channels: { terminals: new Map(), portForwards: new Map() },
          jumpClients: [],
        };
      }
      if (profileId === "disconnected-profile") {
        return {
          profile: { id: "disconnected-profile", host: "offline.example.com" },
          client: mockClient,
          status: "disconnected" as const,
          channels: { terminals: new Map(), portForwards: new Map() },
          jumpClients: [],
        };
      }
      return undefined;
    }),
    on: vi.fn(),
    emit: vi.fn(),
  };
  return { manager, mockClient };
}

// ─── Import under test ───────────────────────────────────────────────────────

import { RemoteTerminalManager } from "../electron/ssh-terminal";
import type { SshSessionManager } from "../electron/ssh-manager";

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("RemoteTerminalManager", () => {
  let terminalManager: RemoteTerminalManager;
  let mockClient: MockClient;
  let mockChannel: MockClientChannel;

  beforeEach(() => {
    mockChannel = new MockClientChannel();
    const { manager, mockClient: mc } = createMockSessionManager();
    mockClient = mc;
    mockClient.shell.mockImplementation((_opts: unknown, cb: (err: Error | null, stream: MockClientChannel) => void) => {
      cb(null, mockChannel);
    });
    terminalManager = new RemoteTerminalManager(manager as unknown as SshSessionManager);
  });

  describe("openTerminal", () => {
    it("opens a shell channel with PTY allocation", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 120, 40);

      expect(terminalId).toBeDefined();
      expect(typeof terminalId).toBe("string");
      expect(mockClient.shell).toHaveBeenCalledWith(
        { term: "xterm-256color", cols: 120, rows: 40 },
        expect.any(Function),
      );
    });

    it("sends cd command when cwd is provided", async () => {
      await terminalManager.openTerminal("connected-profile", 80, 24, "/home/user/project");

      expect(mockChannel.write).toHaveBeenCalledWith("cd '/home/user/project'\n");
    });

    it("does not send cd when cwd is not provided", async () => {
      await terminalManager.openTerminal("connected-profile", 80, 24);

      expect(mockChannel.write).not.toHaveBeenCalled();
    });

    it("throws when profile is not connected", async () => {
      await expect(
        terminalManager.openTerminal("disconnected-profile", 80, 24),
      ).rejects.toThrow("not active");
    });

    it("throws when profile does not exist", async () => {
      await expect(
        terminalManager.openTerminal("nonexistent-profile", 80, 24),
      ).rejects.toThrow("not active");
    });

    it("emits terminal-data when channel receives data", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      const dataPromise = new Promise<{ terminalId: string; data: string }>((resolve) => {
        terminalManager.on("ssh:terminal-data", resolve);
      });

      mockChannel.emit("data", Buffer.from("hello world"));

      const event = await dataPromise;
      expect(event.terminalId).toBe(terminalId);
      expect(event.data).toBe("hello world");
    });

    it("escapes single quotes in cwd path", async () => {
      await terminalManager.openTerminal("connected-profile", 80, 24, "/home/user/it's a dir");

      expect(mockChannel.write).toHaveBeenCalledWith("cd '/home/user/it'\\''s a dir'\n");
    });
  });

  describe("writeToTerminal", () => {
    it("writes data to the SSH channel", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      terminalManager.writeToTerminal(terminalId, "ls -la\n");

      expect(mockChannel.write).toHaveBeenCalledWith("ls -la\n");
    });

    it("throws for unknown terminal ID", () => {
      expect(() => terminalManager.writeToTerminal("nonexistent", "data")).toThrow(
        "Terminal not found",
      );
    });

    it("swallows input for dead session awaiting reconnect", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      // Simulate channel death
      mockChannel.emit("close");

      // Wait for event processing
      await new Promise((r) => setTimeout(r, 10));

      // Should not throw — input is swallowed while awaiting reconnect
      expect(() => terminalManager.writeToTerminal(terminalId, "data")).not.toThrow();
      // The write should not reach the channel (it's dead)
      const writeCallsAfterClose = mockChannel.write.mock.calls.length;
      terminalManager.writeToTerminal(terminalId, "more data");
      expect(mockChannel.write.mock.calls.length).toBe(writeCallsAfterClose);
    });
  });

  describe("resizeTerminal", () => {
    it("calls setWindow with correct parameters", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      terminalManager.resizeTerminal(terminalId, 120, 40);

      // setWindow takes (rows, cols, height, width)
      expect(mockChannel.setWindow).toHaveBeenCalledWith(40, 120, 0, 0);
    });

    it("updates stored dimensions", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      terminalManager.resizeTerminal(terminalId, 120, 40);

      const session = terminalManager.getSession(terminalId);
      expect(session?.cols).toBe(120);
      expect(session?.rows).toBe(40);
    });

    it("throws for unknown terminal ID", () => {
      expect(() => terminalManager.resizeTerminal("nonexistent", 80, 24)).toThrow(
        "Terminal not found",
      );
    });

    it("silently ignores resize on dead session", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      // Should not throw
      terminalManager.resizeTerminal(terminalId, 120, 40);
    });
  });

  describe("closeTerminal", () => {
    it("ends the SSH channel", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      terminalManager.closeTerminal(terminalId);

      expect(mockChannel.end).toHaveBeenCalled();
    });

    it("emits terminal-closed event", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      const closedPromise = new Promise<{ terminalId: string; profileId: string }>((resolve) => {
        terminalManager.on("ssh:terminal-closed", resolve);
      });

      terminalManager.closeTerminal(terminalId);

      const event = await closedPromise;
      expect(event.terminalId).toBe(terminalId);
      expect(event.profileId).toBe("connected-profile");
    });

    it("removes session from map", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      terminalManager.closeTerminal(terminalId);

      expect(terminalManager.getSession(terminalId)).toBeUndefined();
    });

    it("is idempotent (no error on double close)", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      terminalManager.closeTerminal(terminalId);
      terminalManager.closeTerminal(terminalId); // Should not throw
    });
  });

  describe("session persistence and reconnection", () => {
    it("emits session-ended when channel closes unexpectedly", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      const endedPromise = new Promise<{ terminalId: string; profileId: string }>((resolve) => {
        terminalManager.on("ssh:terminal-session-ended", resolve);
      });

      mockChannel.emit("close");

      const event = await endedPromise;
      expect(event.terminalId).toBe(terminalId);
      expect(event.profileId).toBe("connected-profile");
    });

    it("marks session as dead on channel close", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      const session = terminalManager.getSession(terminalId);
      expect(session?.alive).toBe(false);
      expect(session?.channel).toBeNull();
    });

    it("reconnects a dead session with a new channel", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24, "/home/user");

      // Kill the session
      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      // Create a new mock channel for reconnection
      const newChannel = new MockClientChannel();
      mockClient.shell.mockImplementation((_opts: unknown, cb: (err: Error | null, stream: MockClientChannel) => void) => {
        cb(null, newChannel);
      });

      const reconnectedId = await terminalManager.reconnectTerminal(terminalId);

      expect(reconnectedId).toBe(terminalId);
      const session = terminalManager.getSession(terminalId);
      expect(session?.alive).toBe(true);
      expect(session?.channel).toBe(newChannel);
      // Should cd back to original cwd
      expect(newChannel.write).toHaveBeenCalledWith("cd '/home/user'\n");
    });

    it("throws on reconnect when connection is not available", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      // Make the connection unavailable
      const { manager } = createMockSessionManager();
      manager.getConnection.mockReturnValue(undefined);
      const newTerminalManager = new RemoteTerminalManager(manager as unknown as SshSessionManager);

      // We need to test with the original manager but make it return disconnected
      const { manager: disconnectedManager } = createMockSessionManager();
      disconnectedManager.getConnection.mockReturnValue({
        profile: { id: "connected-profile", host: "test.example.com" },
        client: mockClient,
        status: "disconnected",
        channels: { terminals: new Map(), portForwards: new Map() },
        jumpClients: [],
      });

      // Replace the session manager reference
      const mgr = new RemoteTerminalManager(disconnectedManager as unknown as SshSessionManager);
      // Open a terminal first
      const newChannel = new MockClientChannel();
      const newMockClient = new MockClient();
      newMockClient.shell.mockImplementation((_opts: unknown, cb: (err: Error | null, stream: MockClientChannel) => void) => {
        cb(null, newChannel);
      });
      disconnectedManager.getConnection.mockReturnValueOnce({
        profile: { id: "connected-profile", host: "test.example.com" },
        client: newMockClient,
        status: "connected",
        channels: { terminals: new Map(), portForwards: new Map() },
        jumpClients: [],
      });
      const tid = await mgr.openTerminal("connected-profile", 80, 24);
      newChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      await expect(mgr.reconnectTerminal(tid)).rejects.toThrow("not active");
    });
  });

  describe("getSessionsForProfile", () => {
    it("returns all sessions for a profile", async () => {
      await terminalManager.openTerminal("connected-profile", 80, 24);
      await terminalManager.openTerminal("connected-profile", 120, 40);

      const sessions = terminalManager.getSessionsForProfile("connected-profile");
      expect(sessions).toHaveLength(2);
    });

    it("returns empty array for unknown profile", () => {
      const sessions = terminalManager.getSessionsForProfile("unknown");
      expect(sessions).toHaveLength(0);
    });
  });

  describe("closeAllForProfile", () => {
    it("closes all terminals for a profile", async () => {
      const id1 = await terminalManager.openTerminal("connected-profile", 80, 24);
      const id2 = await terminalManager.openTerminal("connected-profile", 120, 40);

      terminalManager.closeAllForProfile("connected-profile");

      expect(terminalManager.getSession(id1)).toBeUndefined();
      expect(terminalManager.getSession(id2)).toBeUndefined();
    });
  });

  describe("session recovery (probing after reconnection)", () => {
    it("probeTerminalSession returns true when channel responds with data", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const session = terminalManager.getSession(terminalId)!;

      // Simulate the channel responding to the probe
      const probePromise = terminalManager.probeTerminalSession(session);
      // The probe sends a null byte; simulate the shell echoing back
      setTimeout(() => mockChannel.emit("data", Buffer.from("response")), 50);

      const result = await probePromise;
      expect(result).toBe(true);
    });

    it("probeTerminalSession returns false when channel does not respond within timeout", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const session = terminalManager.getSession(terminalId)!;

      // Don't emit any data — let the probe timeout
      // Use a shorter timeout for testing by directly calling the method
      const result = await terminalManager.probeTerminalSession(session);
      // This will timeout after PROBE_TIMEOUT_MS (2000ms)
      expect(result).toBe(false);
    }, 5000);

    it("probeTerminalSession returns false when channel is null", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const session = terminalManager.getSession(terminalId)!;
      session.channel = null;
      session.alive = false;

      const result = await terminalManager.probeTerminalSession(session);
      expect(result).toBe(false);
    });

    it("probeTerminalSession returns false when channel emits close", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const session = terminalManager.getSession(terminalId)!;

      const probePromise = terminalManager.probeTerminalSession(session);
      setTimeout(() => mockChannel.emit("close"), 50);

      const result = await probePromise;
      expect(result).toBe(false);
    });

    it("probeTerminalSession returns false when channel emits error", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const session = terminalManager.getSession(terminalId)!;

      const probePromise = terminalManager.probeTerminalSession(session);
      setTimeout(() => mockChannel.emit("error", new Error("channel error")), 50);

      const result = await probePromise;
      expect(result).toBe(false);
    });

    it("handleReconnection emits terminal-resumed for alive sessions", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const events: Array<{ terminalId: string; profileId: string }> = [];
      terminalManager.on("ssh:terminal-resumed", (payload) => events.push(payload));

      // Simulate the channel responding to the probe
      const handlePromise = terminalManager.handleReconnection("connected-profile");
      setTimeout(() => mockChannel.emit("data", Buffer.from("alive")), 50);

      await handlePromise;
      expect(events).toHaveLength(1);
      expect(events[0].terminalId).toBe(terminalId);
      expect(events[0].profileId).toBe("connected-profile");
    });

    it("handleReconnection emits terminal-session-ended for dead sessions", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);
      const events: Array<{ terminalId: string; profileId: string }> = [];
      terminalManager.on("ssh:terminal-session-ended", (payload) => events.push(payload));

      // Simulate the channel closing (dead) during the probe.
      // The channel's own close handler fires first (from openTerminal),
      // which sets awaitingReconnect=true and emits session-ended.
      // handleReconnection then sees awaitingReconnect is already true
      // and does NOT emit a duplicate event.
      const handlePromise = terminalManager.handleReconnection("connected-profile");
      setTimeout(() => mockChannel.emit("close"), 50);

      await handlePromise;
      // Only 1 event from the channel close handler (not duplicated by handleReconnection)
      expect(events).toHaveLength(1);
      expect(events[0].terminalId).toBe(terminalId);

      // Session should be marked as awaiting reconnect
      const session = terminalManager.getSession(terminalId)!;
      expect(session.awaitingReconnect).toBe(true);
      expect(session.alive).toBe(false);
    });

    it("writeToTerminal triggers reconnection on Enter when awaiting reconnect", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      // Simulate channel death
      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      const session = terminalManager.getSession(terminalId)!;
      expect(session.awaitingReconnect).toBe(true);

      // Set up a new channel for the reconnection
      const newChannel = new MockClientChannel();
      mockClient.shell.mockImplementation((_opts: unknown, cb: (err: Error | null, stream: MockClientChannel) => void) => {
        cb(null, newChannel);
      });

      // Press Enter — should trigger reconnection
      terminalManager.writeToTerminal(terminalId, "\r");

      // Wait for the async reconnection to complete
      await new Promise((r) => setTimeout(r, 10));

      // Session should be alive again
      expect(session.alive).toBe(true);
      expect(session.awaitingReconnect).toBe(false);
    });

    it("writeToTerminal swallows non-Enter input when awaiting reconnect", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      // Simulate channel death
      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      const session = terminalManager.getSession(terminalId)!;
      expect(session.awaitingReconnect).toBe(true);

      // Non-Enter input should be swallowed without error
      expect(() => terminalManager.writeToTerminal(terminalId, "a")).not.toThrow();
      expect(() => terminalManager.writeToTerminal(terminalId, "hello")).not.toThrow();

      // Session should still be awaiting reconnect
      expect(session.awaitingReconnect).toBe(true);
    });

    it("handleReconnection does nothing for profiles with no sessions", async () => {
      const events: unknown[] = [];
      terminalManager.on("ssh:terminal-resumed", (p) => events.push(p));
      terminalManager.on("ssh:terminal-session-ended", (p) => events.push(p));

      await terminalManager.handleReconnection("unknown-profile");
      expect(events).toHaveLength(0);
    });

    it("sets awaitingReconnect on channel close in openTerminal", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      const session = terminalManager.getSession(terminalId)!;
      expect(session.awaitingReconnect).toBe(true);
      expect(session.alive).toBe(false);
      expect(session.channel).toBeNull();
    });

    it("clears awaitingReconnect after successful reconnection", async () => {
      const terminalId = await terminalManager.openTerminal("connected-profile", 80, 24);

      // Kill the session
      mockChannel.emit("close");
      await new Promise((r) => setTimeout(r, 10));

      // Set up a new channel for reconnection
      const newChannel = new MockClientChannel();
      mockClient.shell.mockImplementation((_opts: unknown, cb: (err: Error | null, stream: MockClientChannel) => void) => {
        cb(null, newChannel);
      });

      await terminalManager.reconnectTerminal(terminalId);

      const session = terminalManager.getSession(terminalId)!;
      expect(session.awaitingReconnect).toBe(false);
      expect(session.alive).toBe(true);
      expect(session.channel).toBe(newChannel);
    });
  });
});
