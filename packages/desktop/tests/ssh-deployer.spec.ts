/**
 * Unit/integration tests for the AgentDeployer class.
 *
 * Mocks the ssh2 Client and fs modules to test architecture detection,
 * version comparison, hash computation, agent startup, and error handling
 * without real SSH connections.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import * as crypto from "node:crypto";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock fs for binary existence checks and hash computation
vi.mock("node:fs", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:fs")>();
  return {
    ...actual,
    existsSync: vi.fn(() => true),
    readFileSync: vi.fn((p: unknown) => {
      if (typeof p === "string" && p.includes("agents")) {
        return Buffer.from("mock-binary-content");
      }
      if (typeof p === "string" && p.includes("package.json")) {
        return JSON.stringify({ version: "0.1.0" });
      }
      return actual.readFileSync(p as string);
    }),
    statSync: vi.fn(() => ({ size: 1024 * 1024 })), // 1MB
  };
});

// Mock ssh2 Client
class MockClientChannel extends EventEmitter {
  end = vi.fn();
  write = vi.fn();
  stderr = new EventEmitter();
}

class MockSFTPWrapper extends EventEmitter {
  realpath = vi.fn();
  mkdir = vi.fn();
  stat = vi.fn();
  chmod = vi.fn();
  createWriteStream = vi.fn();
  end = vi.fn();
}

class MockClient extends EventEmitter {
  connect = vi.fn();
  end = vi.fn();
  exec = vi.fn();
  sftp = vi.fn();
}

vi.mock("ssh2", () => ({
  Client: vi.fn(() => new MockClient()),
}));

import { AgentDeployer, DeployError } from "../electron/ssh-deployer";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createMockClient(): MockClient {
  return new MockClient();
}

/**
 * Set up the mock client's exec to return specific stdout output.
 */
function mockExecOutput(client: MockClient, output: string, exitCode = 0): void {
  client.exec.mockImplementation((_cmd: string, ...args: unknown[]) => {
    // Handle both (cmd, cb) and (cmd, opts, cb) signatures
    const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
    const stream = new MockClientChannel();
    cb(null, stream);
    setImmediate(() => {
      stream.emit("data", Buffer.from(output));
      stream.emit("close", exitCode);
    });
  });
}

/**
 * Set up the mock client's exec to return an error.
 */
function mockExecError(client: MockClient, error: Error): void {
  client.exec.mockImplementation((_cmd: string, ...args: unknown[]) => {
    const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
    cb(error);
  });
}

/**
 * Set up the mock client's exec to return different outputs for different commands.
 */
function mockExecMultiple(client: MockClient, responses: Record<string, { output: string; exitCode?: number }>): void {
  client.exec.mockImplementation((cmd: string, ...args: unknown[]) => {
    const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
    const stream = new MockClientChannel();
    cb(null, stream);

    // Find matching response
    let matched = false;
    for (const [pattern, response] of Object.entries(responses)) {
      if (cmd.includes(pattern)) {
        setImmediate(() => {
          stream.emit("data", Buffer.from(response.output));
          stream.emit("close", response.exitCode ?? 0);
        });
        matched = true;
        break;
      }
    }

    if (!matched) {
      setImmediate(() => {
        stream.emit("data", Buffer.from(""));
        stream.emit("close", 0);
      });
    }
  });
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("AgentDeployer", () => {
  let deployer: AgentDeployer;

  beforeEach(() => {
    deployer = new AgentDeployer();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  // ── detectRemoteArch ─────────────────────────────────────────────

  describe("detectRemoteArch", () => {
    it("maps Linux x86_64 to the correct binary name", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Linux\nx86_64\n");

      const result = await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);
      expect(result).toBe("crab-desktop-agent-x86_64-linux");
    });

    it("maps Linux aarch64 to the correct binary name", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Linux\naarch64\n");

      const result = await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);
      expect(result).toBe("crab-desktop-agent-aarch64-linux");
    });

    it("maps Linux arm64 to the aarch64 binary name", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Linux\narm64\n");

      const result = await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);
      expect(result).toBe("crab-desktop-agent-aarch64-linux");
    });

    it("maps Darwin arm64 to the correct binary name", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Darwin\narm64\n");

      const result = await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);
      expect(result).toBe("crab-desktop-agent-aarch64-darwin");
    });

    it("maps Darwin aarch64 to the correct binary name", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Darwin\naarch64\n");

      const result = await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);
      expect(result).toBe("crab-desktop-agent-aarch64-darwin");
    });

    it("throws DeployError with WRONG_ARCH for unsupported OS", async () => {
      const client = createMockClient();
      mockExecOutput(client, "FreeBSD\nx86_64\n");

      await expect(
        deployer.detectRemoteArch(client as unknown as import("ssh2").Client),
      ).rejects.toThrow(DeployError);

      await expect(
        deployer.detectRemoteArch(client as unknown as import("ssh2").Client),
      ).rejects.toMatchObject({ code: "WRONG_ARCH" });
    });

    it("throws DeployError with WRONG_ARCH for unsupported architecture", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Linux\nmips64\n");

      await expect(
        deployer.detectRemoteArch(client as unknown as import("ssh2").Client),
      ).rejects.toThrow(DeployError);

      await expect(
        deployer.detectRemoteArch(client as unknown as import("ssh2").Client),
      ).rejects.toMatchObject({ code: "WRONG_ARCH" });
    });

    it("throws DeployError with ARCH_DETECT_FAILED for malformed uname output", async () => {
      const client = createMockClient();
      mockExecOutput(client, "incomplete\n");

      await expect(
        deployer.detectRemoteArch(client as unknown as import("ssh2").Client),
      ).rejects.toMatchObject({ code: "ARCH_DETECT_FAILED" });
    });
  });

  // ── checkRemoteAgent ─────────────────────────────────────────────

  describe("checkRemoteAgent", () => {
    it("returns version string when agent is installed", async () => {
      const client = createMockClient();
      mockExecOutput(client, "crab-desktop-agent 0.1.0\n");

      const result = await deployer.checkRemoteAgent(client as unknown as import("ssh2").Client);
      expect(result).toBe("0.1.0");
    });

    it("returns version from bare version output", async () => {
      const client = createMockClient();
      mockExecOutput(client, "0.2.3\n");

      const result = await deployer.checkRemoteAgent(client as unknown as import("ssh2").Client);
      expect(result).toBe("0.2.3");
    });

    it("returns null when agent is not found", async () => {
      const client = createMockClient();
      mockExecOutput(client, "bash: ~/.crab/bin/crab-desktop-agent: No such file or directory\n", 127);

      const result = await deployer.checkRemoteAgent(client as unknown as import("ssh2").Client);
      expect(result).toBeNull();
    });

    it("returns null when command not found", async () => {
      const client = createMockClient();
      mockExecOutput(client, "command not found\n", 127);

      const result = await deployer.checkRemoteAgent(client as unknown as import("ssh2").Client);
      expect(result).toBeNull();
    });

    it("returns null when exec throws an error", async () => {
      const client = createMockClient();
      mockExecError(client, new Error("channel open failed"));

      const result = await deployer.checkRemoteAgent(client as unknown as import("ssh2").Client);
      expect(result).toBeNull();
    });
  });

  // ── compareVersions ──────────────────────────────────────────────

  describe("compareVersions", () => {
    it("returns 'current' when versions are identical", () => {
      expect(deployer.compareVersions("0.1.0", "0.1.0")).toBe("current");
      expect(deployer.compareVersions("1.2.3", "1.2.3")).toBe("current");
    });

    it("returns 'upgrade' when local is newer (major)", () => {
      expect(deployer.compareVersions("2.0.0", "1.0.0")).toBe("upgrade");
    });

    it("returns 'upgrade' when local is newer (minor)", () => {
      expect(deployer.compareVersions("0.2.0", "0.1.0")).toBe("upgrade");
    });

    it("returns 'upgrade' when local is newer (patch)", () => {
      expect(deployer.compareVersions("0.1.1", "0.1.0")).toBe("upgrade");
    });

    it("returns 'downgrade' when remote is newer (major)", () => {
      expect(deployer.compareVersions("1.0.0", "2.0.0")).toBe("downgrade");
    });

    it("returns 'downgrade' when remote is newer (minor)", () => {
      expect(deployer.compareVersions("0.1.0", "0.2.0")).toBe("downgrade");
    });

    it("returns 'downgrade' when remote is newer (patch)", () => {
      expect(deployer.compareVersions("0.1.0", "0.1.1")).toBe("downgrade");
    });

    it("handles multi-digit version components", () => {
      expect(deployer.compareVersions("1.10.0", "1.9.0")).toBe("upgrade");
      expect(deployer.compareVersions("1.9.0", "1.10.0")).toBe("downgrade");
    });
  });

  // ── computeLocalHash ─────────────────────────────────────────────

  describe("computeLocalHash", () => {
    it("produces a consistent SHA-256 hash", () => {
      const hash1 = deployer.computeLocalHash("/path/to/agents/binary");
      const hash2 = deployer.computeLocalHash("/path/to/agents/binary");
      expect(hash1).toBe(hash2);
    });

    it("returns a valid 64-character hex string", () => {
      const hash = deployer.computeLocalHash("/path/to/agents/binary");
      expect(hash).toMatch(/^[a-f0-9]{64}$/);
    });

    it("matches expected SHA-256 of mock content", () => {
      // The mock fs.readFileSync returns Buffer.from("mock-binary-content")
      const expected = crypto
        .createHash("sha256")
        .update(Buffer.from("mock-binary-content"))
        .digest("hex");
      const hash = deployer.computeLocalHash("/path/to/agents/binary");
      expect(hash).toBe(expected);
    });
  });

  // ── startRemoteAgent ─────────────────────────────────────────────

  describe("startRemoteAgent", () => {
    it("sends ping RPC and resolves on valid response", async () => {
      const client = createMockClient();

      client.exec.mockImplementation((_cmd: string, ...args: unknown[]) => {
        const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
        const channel = new MockClientChannel();
        cb(null, channel);

        // Simulate the agent responding to the ping after a short delay
        setImmediate(() => {
          const response = JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            result: { pong: "0.1.0" },
          });
          channel.emit("data", Buffer.from(response + "\n"));
        });
      });

      const channel = await deployer.startRemoteAgent(client as unknown as import("ssh2").Client);
      expect(channel).toBeDefined();
      expect(channel.write).toHaveBeenCalledWith(
        expect.stringContaining('"method":"ping"'),
      );
    });

    it("rejects when agent does not respond (channel closes)", async () => {
      const client = createMockClient();

      client.exec.mockImplementation((_cmd: string, ...args: unknown[]) => {
        const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
        const channel = new MockClientChannel();
        cb(null, channel);

        // Simulate the channel closing without a response
        setImmediate(() => {
          channel.emit("close");
        });
      });

      await expect(
        deployer.startRemoteAgent(client as unknown as import("ssh2").Client),
      ).rejects.toThrow(DeployError);

      await expect(
        deployer.startRemoteAgent(client as unknown as import("ssh2").Client),
      ).rejects.toMatchObject({ code: "START_FAILED" });
    });

    it("rejects when exec fails", async () => {
      const client = createMockClient();
      mockExecError(client, new Error("channel open failed"));

      await expect(
        deployer.startRemoteAgent(client as unknown as import("ssh2").Client),
      ).rejects.toThrow(DeployError);
    });

    it("rejects when agent returns an error response to ping", async () => {
      const client = createMockClient();

      client.exec.mockImplementation((_cmd: string, ...args: unknown[]) => {
        const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
        const channel = new MockClientChannel();
        cb(null, channel);

        setImmediate(() => {
          const response = JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            error: { code: -32601, message: "Method not found" },
          });
          channel.emit("data", Buffer.from(response + "\n"));
        });
      });

      await expect(
        deployer.startRemoteAgent(client as unknown as import("ssh2").Client),
      ).rejects.toMatchObject({ code: "START_FAILED" });
    });
  });

  // ── Error handling ───────────────────────────────────────────────

  describe("error handling", () => {
    it("DISK_FULL: checkDiskSpace throws when remote disk is full", async () => {
      const client = createMockClient();

      // deploy() calls execCommand twice sequentially:
      // 1. checkBinaryNotRunning: "pgrep -f crab-desktop-agent 2>/dev/null || true"
      // 2. checkDiskSpace: "df -k ~ 2>/dev/null | tail -1"
      let callCount = 0;
      client.exec.mockImplementation((cmd: string, ...args: unknown[]) => {
        const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
        const stream = new MockClientChannel();
        cb(null, stream);
        callCount++;

        setImmediate(() => {
          if (cmd.includes("pgrep")) {
            // No running agent
            stream.emit("data", Buffer.from(""));
            stream.emit("close", 1);
          } else if (cmd.includes("df")) {
            // Very low disk space (1KB available)
            stream.emit("data", Buffer.from("/dev/sda1 100000 99999 1 100% /home\n"));
            stream.emit("close", 0);
          } else {
            stream.emit("data", Buffer.from(""));
            stream.emit("close", 0);
          }
        });
      });

      await expect(
        deployer.deploy(client as unknown as import("ssh2").Client, "crab-desktop-agent-x86_64-linux"),
      ).rejects.toMatchObject({ code: "DISK_FULL" });
    });

    it("PERMISSION_DENIED: deploy throws when SFTP channel fails", async () => {
      const client = createMockClient();

      // Mock pgrep (no running agent) and df (enough space)
      mockExecMultiple(client, {
        pgrep: { output: "", exitCode: 0 },
        "df -k": { output: "Filesystem 1K-blocks Used Available Use% Mounted\n/dev/sda1 100000000 50000000 50000000 50% /home\n", exitCode: 0 },
      });

      // Mock sftp to fail with permission error
      client.sftp.mockImplementation((cb: Function) => {
        cb(new Error("Permission denied"));
      });

      await expect(
        deployer.deploy(client as unknown as import("ssh2").Client, "crab-desktop-agent-x86_64-linux"),
      ).rejects.toMatchObject({ code: "UPLOAD_FAILED" });
    });

    it("BINARY_RUNNING: deploy throws when agent is already running", async () => {
      const client = createMockClient();

      // Mock pgrep to return a PID (agent is running)
      mockExecMultiple(client, {
        pgrep: { output: "12345\n", exitCode: 0 },
      });

      await expect(
        deployer.deploy(client as unknown as import("ssh2").Client, "crab-desktop-agent-x86_64-linux"),
      ).rejects.toMatchObject({ code: "BINARY_RUNNING" });
    });

    it("WRONG_ARCH: detectRemoteArch throws for unsupported platform", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Windows_NT\nx86_64\n");

      await expect(
        deployer.detectRemoteArch(client as unknown as import("ssh2").Client),
      ).rejects.toMatchObject({ code: "WRONG_ARCH" });
    });

    it("DeployError includes suggestion field", async () => {
      const client = createMockClient();
      mockExecOutput(client, "FreeBSD\namd64\n");

      try {
        await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);
        expect.fail("Should have thrown");
      } catch (err) {
        expect(err).toBeInstanceOf(DeployError);
        expect((err as DeployError).suggestion).toBeDefined();
        expect((err as DeployError).suggestion!.length).toBeGreaterThan(0);
      }
    });
  });

  // ── Progress events ──────────────────────────────────────────────

  describe("progress events", () => {
    it("emits ssh:deploy-progress during detectRemoteArch", async () => {
      const client = createMockClient();
      mockExecOutput(client, "Linux\nx86_64\n");

      const events: unknown[] = [];
      deployer.on("ssh:deploy-progress", (p) => events.push(p));

      await deployer.detectRemoteArch(client as unknown as import("ssh2").Client);

      expect(events.length).toBeGreaterThanOrEqual(2);
      expect(events[0]).toMatchObject({ stage: "detecting", percent: 0 });
      expect(events[events.length - 1]).toMatchObject({ stage: "detecting", percent: 100 });
    });

    it("emits ssh:deploy-progress during startRemoteAgent", async () => {
      const client = createMockClient();

      client.exec.mockImplementation((_cmd: string, ...args: unknown[]) => {
        const cb = typeof args[0] === "function" ? args[0] as Function : args[1] as Function;
        const channel = new MockClientChannel();
        cb(null, channel);

        setImmediate(() => {
          const response = JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            result: { pong: "0.1.0" },
          });
          channel.emit("data", Buffer.from(response + "\n"));
        });
      });

      const events: unknown[] = [];
      deployer.on("ssh:deploy-progress", (p) => events.push(p));

      await deployer.startRemoteAgent(client as unknown as import("ssh2").Client);

      expect(events).toContainEqual(expect.objectContaining({ stage: "starting", percent: 0 }));
      expect(events).toContainEqual(expect.objectContaining({ stage: "starting", percent: 100 }));
    });
  });
});
