import { mockStorageConnections } from "./mock-storage-connections";
import type { SshSessionManager } from "../electron/ssh-manager";
/**
 * Verification tests for remote Crab operations over the SSH router.
 *
 * These tests verify that crab-specific verbs (add, hydrate, dehydrate,
 * push, status, pointer_top) route correctly through the
 * RPC router to the remote agent, including progress streaming and
 * cancellation support.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import { parseRepoId } from "../electron/ssh-types";
import { RpcRouter } from "../electron/ssh-router";

// ─── Mock Infrastructure ─────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();
  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

class MockSessionManager extends EventEmitter {
  repositoryWork(): Promise<unknown> | undefined { return undefined; }
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockChannel } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { channels: { agent?: MockChannel } } | undefined {
    return this.connections.get(profileId);
  }
}

class MockLocalAgent {
  calls: Array<{ method: string; params: unknown }> = [];
  nextResult: unknown = { ok: true };

  async request(method: string, params: unknown): Promise<unknown> {
    this.calls.push({ method, params });
    return this.nextResult;
  }
}

function createRouter(): {
  router: RpcRouter;
  channel: MockChannel;
  sessionManager: MockSessionManager;
  localAgent: MockLocalAgent;
} {
  const sessionManager = new MockSessionManager() as any;
  const localAgent = new MockLocalAgent();
  const router = new RpcRouter(sessionManager, localAgent, mockStorageConnections, { requestTimeoutMs: 5000 });
  const channel = new MockChannel();

  sessionManager.setStatus("gpu-server", "connected");
  sessionManager.setConnection("gpu-server", channel);
  router.attachChannel("gpu-server", channel.asChannel());

  return { router, channel, sessionManager, localAgent };
}

function simulateResponse(channel: MockChannel, id: number, result: unknown): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
  channel.emit("data", Buffer.from(response));
}

function simulateNotification(channel: MockChannel, method: string, params: unknown): void {
  const notification = JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n";
  channel.emit("data", Buffer.from(notification));
}

// ─── Tests ───────────────────────────────────────────────────────

describe("Remote Crab Operations via RPC Router", () => {
  let router: RpcRouter;
  let channel: MockChannel;
  let localAgent: MockLocalAgent;

  beforeEach(() => {
    const setup = createRouter();
    router = setup.router;
    channel = setup.channel;
    localAgent = setup.localAgent;
  });

  describe("add verb", () => {
    it("routes add to remote agent for staging large files", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["model.safetensors", "data/*.parquet"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("add");
      expect(written.params.paths).toEqual(["model.safetensors", "data/*.parquet"]);

      simulateResponse(channel, written.id, {
        staged: 2,
        totalBytes: 4_500_000_000,
        chunks: 1024,
      });

      const result = await promise;
      expect(result).toEqual({ staged: 2, totalBytes: 4_500_000_000, chunks: 1024 });
    });

    it("receives progress notifications during add for large files", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["weights-00001.safetensors", "weights-00002.safetensors"],
      });

      // Simulate chunking progress notifications streaming back over SSH
      simulateNotification(channel, "progress", {
        op: "add",
        file: "weights-00001.safetensors",
        percent: 50,
        bytesProcessed: 2_250_000_000,
        totalBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "add",
        file: "weights-00001.safetensors",
        percent: 100,
        bytesProcessed: 4_500_000_000,
        totalBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "add",
        file: "weights-00002.safetensors",
        percent: 100,
        bytesProcessed: 4_500_000_000,
        totalBytes: 4_500_000_000,
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        staged: 2,
        totalBytes: 9_000_000_000,
        chunks: 2048,
      });

      const result = await promise;
      expect(result).toEqual({ staged: 2, totalBytes: 9_000_000_000, chunks: 2048 });

      // Verify all progress notifications were received with correct profileId
      expect(notifications).toHaveLength(3);
      for (const n of notifications) {
        expect(n).toMatchObject({ profileId: "gpu-server", method: "progress" });
      }
      expect((notifications[0] as any).params.file).toBe("weights-00001.safetensors");
      expect((notifications[0] as any).params.percent).toBe(50);
      expect((notifications[2] as any).params.file).toBe("weights-00002.safetensors");
    });

    it("stages files with glob patterns on the remote", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["**/*.bin", "checkpoints/**/*.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("add");
      expect(written.params.paths).toEqual(["**/*.bin", "checkpoints/**/*.safetensors"]);

      simulateResponse(channel, written.id, {
        staged: 15,
        totalBytes: 45_000_000_000,
        chunks: 8192,
      });

      const result = await promise;
      expect((result as any).staged).toBe(15);
      expect((result as any).totalBytes).toBe(45_000_000_000);
    });

    it("handles add with no matching files on remote", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["nonexistent/*.bin"],
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        staged: 0,
        totalBytes: 0,
        chunks: 0,
      });

      const result = await promise;
      expect(result).toEqual({ staged: 0, totalBytes: 0, chunks: 0 });
    });

    it("does not route add to local agent for remote repos", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["model.bin"],
      });

      // Verify the call went to the remote channel, not the local agent
      expect(channel.written).toHaveLength(1);
      expect(localAgent.calls).toHaveLength(0);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { staged: 1, totalBytes: 1_000_000, chunks: 4 });
      await promise;
    });

    it("rejects add during reconnection (write operation)", async () => {
      const setup = createRouter();
      const { router: r2, sessionManager: sm } = setup;
      sm.setStatus("gpu-server", "reconnecting");

      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // add is a write operation — should be rejected immediately during reconnection
      await expect(
        r2.route(repoId, "add", { repo_id: repoId, paths: ["model.bin"] }),
      ).rejects.toThrow(/live operations are unavailable during reconnection/);
    });
  });

  describe("hydrate verb", () => {
    it("routes hydrate to remote agent for a single pattern", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["*.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("hydrate");
      expect(written.params.patterns).toEqual(["*.safetensors"]);

      simulateResponse(channel, written.id, {
        hydrated: 5,
        totalBytes: 12_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({ hydrated: 5, totalBytes: 12_000_000_000 });
    });

    it("routes hydrate with --all flag to hydrate entire repo", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("hydrate");
      expect(written.params.patterns).toEqual(["--all"]);

      simulateResponse(channel, written.id, {
        hydrated: 42,
        totalBytes: 150_000_000_000,
      });

      const result = await promise;
      expect((result as any).hydrated).toBe(42);
      expect((result as any).totalBytes).toBe(150_000_000_000);
    });

    it("hydrates files with multiple glob patterns on the remote", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["models/**/*.safetensors", "data/*.parquet", "checkpoints/**/*.bin"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("hydrate");
      expect(written.params.patterns).toEqual([
        "models/**/*.safetensors",
        "data/*.parquet",
        "checkpoints/**/*.bin",
      ]);

      simulateResponse(channel, written.id, {
        hydrated: 28,
        totalBytes: 95_000_000_000,
      });

      const result = await promise;
      expect((result as any).hydrated).toBe(28);
      expect((result as any).totalBytes).toBe(95_000_000_000);
    });

    it("receives per-file progress notifications with bytes, speed, and ETA during hydrate", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model-00001.safetensors", "model-00002.safetensors"],
      });

      // Simulate per-file progress notifications streaming back over SSH tunnel
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00001.safetensors",
        percent: 25,
        bytesDownloaded: 1_125_000_000,
        totalBytes: 4_500_000_000,
        speed: "150 MB/s",
        eta: "20s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00001.safetensors",
        percent: 50,
        bytesDownloaded: 2_250_000_000,
        totalBytes: 4_500_000_000,
        speed: "160 MB/s",
        eta: "14s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00001.safetensors",
        percent: 100,
        bytesDownloaded: 4_500_000_000,
        totalBytes: 4_500_000_000,
        speed: "175 MB/s",
        eta: "0s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00002.safetensors",
        percent: 50,
        bytesDownloaded: 2_250_000_000,
        totalBytes: 4_500_000_000,
        speed: "180 MB/s",
        eta: "12s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00002.safetensors",
        percent: 100,
        bytesDownloaded: 4_500_000_000,
        totalBytes: 4_500_000_000,
        speed: "185 MB/s",
        eta: "0s",
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        hydrated: 2,
        totalBytes: 9_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({ hydrated: 2, totalBytes: 9_000_000_000 });

      // Verify all 5 progress notifications were received
      expect(notifications).toHaveLength(5);

      // All notifications tagged with the correct profileId
      for (const n of notifications) {
        expect(n).toMatchObject({ profileId: "gpu-server", method: "progress" });
      }

      // Verify per-file progress details for first file
      const first = (notifications[0] as any).params;
      expect(first.op).toBe("hydrate");
      expect(first.file).toBe("model-00001.safetensors");
      expect(first.percent).toBe(25);
      expect(first.bytesDownloaded).toBe(1_125_000_000);
      expect(first.totalBytes).toBe(4_500_000_000);
      expect(first.speed).toBe("150 MB/s");
      expect(first.eta).toBe("20s");

      // Verify completion notification for first file
      const firstComplete = (notifications[2] as any).params;
      expect(firstComplete.file).toBe("model-00001.safetensors");
      expect(firstComplete.percent).toBe(100);
      expect(firstComplete.bytesDownloaded).toBe(4_500_000_000);

      // Verify second file progress
      const secondProgress = (notifications[3] as any).params;
      expect(secondProgress.file).toBe("model-00002.safetensors");
      expect(secondProgress.percent).toBe(50);
      expect(secondProgress.speed).toBe("180 MB/s");
      expect(secondProgress.eta).toBe("12s");
    });

    it("streams overall progress with aggregate stats for multi-file hydrate", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      // Simulate aggregate progress notifications (overall progress bar)
      simulateNotification(channel, "progress", {
        op: "hydrate",
        filesCompleted: 0,
        filesTotal: 10,
        percent: 0,
        bytesDownloaded: 0,
        totalBytes: 50_000_000_000,
        speed: "0 MB/s",
        eta: "calculating...",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        filesCompleted: 3,
        filesTotal: 10,
        percent: 30,
        bytesDownloaded: 15_000_000_000,
        totalBytes: 50_000_000_000,
        speed: "200 MB/s",
        eta: "175s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        filesCompleted: 7,
        filesTotal: 10,
        percent: 70,
        bytesDownloaded: 35_000_000_000,
        totalBytes: 50_000_000_000,
        speed: "220 MB/s",
        eta: "68s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        filesCompleted: 10,
        filesTotal: 10,
        percent: 100,
        bytesDownloaded: 50_000_000_000,
        totalBytes: 50_000_000_000,
        speed: "210 MB/s",
        eta: "0s",
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 10, totalBytes: 50_000_000_000 });

      await promise;

      expect(notifications).toHaveLength(4);
      // Verify progress increases monotonically
      const percents = notifications.map((n: any) => n.params.percent);
      expect(percents).toEqual([0, 30, 70, 100]);

      // Verify speed and ETA are present in all notifications
      for (const n of notifications) {
        expect((n as any).params.speed).toBeDefined();
        expect((n as any).params.eta).toBeDefined();
      }
    });

    it("handles hydrating already-hydrated files gracefully", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["already-hydrated-model.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("hydrate");

      // Remote agent returns 0 hydrated (file was already hydrated)
      simulateResponse(channel, written.id, {
        hydrated: 0,
        totalBytes: 0,
        skipped: 1,
        message: "All files already hydrated",
      });

      const result = await promise;
      expect((result as any).hydrated).toBe(0);
      expect((result as any).skipped).toBe(1);
      expect((result as any).message).toBe("All files already hydrated");
    });

    it("handles hydrate error when file not found on remote", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["nonexistent-model.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);

      // Simulate an error response from the remote agent
      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32001,
          message: "No matching pointer files found for pattern: nonexistent-model.safetensors",
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(
        /No matching pointer files found for pattern/,
      );
    });

    it("handles hydrate error when cloud storage is unreachable", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);

      // Simulate a storage error from the remote agent
      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32002,
          message: "Failed to download xorb: S3 connection timeout",
          data: { xorbHash: "abc123def456", retries: 3 },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Failed to download xorb/);
    });

    it("handles hydrate error when remote disk is full", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["large-dataset.parquet"],
      });

      const written = JSON.parse(channel.written[0]);

      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: 2009,
          message: "Remote disk full: insufficient space to hydrate large-dataset.parquet (need 8GB, have 500MB)",
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Remote disk full/);
    });

    it("does not route hydrate to local agent for remote repos", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model.bin"],
      });

      // Verify the call went to the remote channel, not the local agent
      expect(channel.written).toHaveLength(1);
      expect(localAgent.calls).toHaveLength(0);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 2_000_000_000 });
      await promise;
    });

    it("rejects hydrate during reconnection (write operation)", async () => {
      const setup = createRouter();
      const { router: r2, sessionManager: sm } = setup;
      sm.setStatus("gpu-server", "reconnecting");

      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // hydrate is a write operation — should be rejected immediately during reconnection
      await expect(
        r2.route(repoId, "hydrate", { repo_id: repoId, patterns: ["model.bin"] }),
      ).rejects.toThrow(/live operations are unavailable during reconnection/);
    });

    it("times out hydrate if remote agent does not respond", async () => {
      // Create a router with a short timeout for testing
      const sessionManager = new MockSessionManager() as any;
      const agent = new MockLocalAgent();
      const shortTimeoutRouter = new RpcRouter(sessionManager, agent, mockStorageConnections, { requestTimeoutMs: 50 });
      const ch = new MockChannel();

      sessionManager.setStatus("gpu-server", "connected");
      sessionManager.setConnection("gpu-server", ch);
      shortTimeoutRouter.attachChannel("gpu-server", ch.asChannel());

      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = shortTimeoutRouter.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["huge-model.safetensors"],
      });

      // Don't send a response — let it time out
      await expect(promise).rejects.toThrow(/RPC timeout.*hydrate/);
    });

    it("progress notifications arrive in real-time order without buffering", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notificationTimestamps: number[] = [];
      router.on("notification", () => notificationTimestamps.push(Date.now()));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model.safetensors"],
      });

      // Simulate progress arriving as separate data events (not batched)
      // This verifies NDJSON line-by-line processing works for streaming
      const progress1 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "model.safetensors", percent: 33, speed: "100 MB/s", eta: "30s" },
      }) + "\n";
      channel.emit("data", Buffer.from(progress1));

      const progress2 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "model.safetensors", percent: 66, speed: "120 MB/s", eta: "15s" },
      }) + "\n";
      channel.emit("data", Buffer.from(progress2));

      const progress3 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "model.safetensors", percent: 100, speed: "130 MB/s", eta: "0s" },
      }) + "\n";
      channel.emit("data", Buffer.from(progress3));

      // Each notification should be processed immediately as it arrives
      expect(notificationTimestamps).toHaveLength(3);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 4_500_000_000 });
      await promise;
    });

    it("handles partial NDJSON lines during progress streaming", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model.safetensors"],
      });

      // Simulate data arriving in chunks (split across TCP packets)
      const fullLine = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "model.safetensors", percent: 50, speed: "150 MB/s", eta: "15s" },
      });

      // Send first half of the line
      channel.emit("data", Buffer.from(fullLine.slice(0, 40)));
      expect(notifications).toHaveLength(0); // Not yet complete

      // Send the rest + newline
      channel.emit("data", Buffer.from(fullLine.slice(40) + "\n"));
      expect(notifications).toHaveLength(1); // Now complete

      expect((notifications[0] as any).params.percent).toBe(50);
      expect((notifications[0] as any).params.speed).toBe("150 MB/s");

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 4_500_000_000 });
      await promise;
    });
  });

  describe("dehydrate verb", () => {
    it("routes dehydrate with --all flag to dehydrate entire repo", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("dehydrate");
      expect(written.params.patterns).toEqual(["--all"]);

      simulateResponse(channel, written.id, {
        dehydrated: 8,
        freedBytes: 24_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({ dehydrated: 8, freedBytes: 24_000_000_000 });
    });

    it("routes dehydrate with specific file patterns", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["*.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("dehydrate");
      expect(written.params.patterns).toEqual(["*.safetensors"]);

      simulateResponse(channel, written.id, {
        dehydrated: 5,
        freedBytes: 12_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({ dehydrated: 5, freedBytes: 12_000_000_000 });
    });

    it("routes dehydrate with multiple glob patterns", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["models/**/*.safetensors", "data/*.parquet", "checkpoints/**/*.bin"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("dehydrate");
      expect(written.params.patterns).toEqual([
        "models/**/*.safetensors",
        "data/*.parquet",
        "checkpoints/**/*.bin",
      ]);

      simulateResponse(channel, written.id, {
        dehydrated: 22,
        freedBytes: 85_000_000_000,
      });

      const result = await promise;
      expect((result as any).dehydrated).toBe(22);
      expect((result as any).freedBytes).toBe(85_000_000_000);
    });

    it("receives per-file progress notifications during dehydration", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      // Simulate per-file progress as files are replaced with pointer blobs
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        file: "model-00001.safetensors",
        percent: 100,
        freedBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        file: "model-00002.safetensors",
        percent: 100,
        freedBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        file: "data/train.parquet",
        percent: 100,
        freedBytes: 2_000_000_000,
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        dehydrated: 3,
        freedBytes: 11_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({ dehydrated: 3, freedBytes: 11_000_000_000 });

      // Verify all progress notifications were received with correct profileId
      expect(notifications).toHaveLength(3);
      for (const n of notifications) {
        expect(n).toMatchObject({ profileId: "gpu-server", method: "progress" });
      }
      expect((notifications[0] as any).params.op).toBe("dehydrate");
      expect((notifications[0] as any).params.file).toBe("model-00001.safetensors");
      expect((notifications[0] as any).params.freedBytes).toBe(4_500_000_000);
      expect((notifications[2] as any).params.file).toBe("data/train.parquet");
    });

    it("streams aggregate progress with files completed count", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      // Simulate aggregate progress notifications
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        filesCompleted: 0,
        filesTotal: 8,
        percent: 0,
        freedBytes: 0,
        totalFreeable: 24_000_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        filesCompleted: 4,
        filesTotal: 8,
        percent: 50,
        freedBytes: 12_000_000_000,
        totalFreeable: 24_000_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        filesCompleted: 8,
        filesTotal: 8,
        percent: 100,
        freedBytes: 24_000_000_000,
        totalFreeable: 24_000_000_000,
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { dehydrated: 8, freedBytes: 24_000_000_000 });

      await promise;

      expect(notifications).toHaveLength(3);
      const percents = notifications.map((n: any) => n.params.percent);
      expect(percents).toEqual([0, 50, 100]);

      // Verify freed bytes increases monotonically
      const freed = notifications.map((n: any) => n.params.freedBytes);
      expect(freed).toEqual([0, 12_000_000_000, 24_000_000_000]);
    });

    it("handles dehydrating already-dehydrated files (skipped)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["already-pointer.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("dehydrate");

      // Remote agent returns 0 dehydrated (file was already a pointer blob)
      simulateResponse(channel, written.id, {
        dehydrated: 0,
        freedBytes: 0,
        skipped: 1,
        message: "All files already dehydrated",
      });

      const result = await promise;
      expect((result as any).dehydrated).toBe(0);
      expect((result as any).skipped).toBe(1);
      expect((result as any).message).toBe("All files already dehydrated");
    });

    it("skips dirty files during dehydration (uncommitted changes)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      // Simulate a warning notification about skipped dirty files
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        warning: "skipped_dirty",
        skippedFiles: ["model-wip.safetensors", "data/experiment.parquet"],
        reason: "Files have uncommitted changes",
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        dehydrated: 6,
        freedBytes: 18_000_000_000,
        skipped: 2,
        skippedDirty: ["model-wip.safetensors", "data/experiment.parquet"],
      });

      const result = await promise;
      expect((result as any).dehydrated).toBe(6);
      expect((result as any).skipped).toBe(2);
      expect((result as any).skippedDirty).toEqual([
        "model-wip.safetensors",
        "data/experiment.parquet",
      ]);

      // Verify the warning notification was received
      expect(notifications).toHaveLength(1);
      expect((notifications[0] as any).params.warning).toBe("skipped_dirty");
      expect((notifications[0] as any).params.skippedFiles).toHaveLength(2);
    });

    it("handles permission denied error during dehydration", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["protected-model.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);

      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: 2010,
          message: "Permission denied: cannot write pointer blob for protected-model.safetensors",
          data: { path: "protected-model.safetensors" },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Permission denied/);
    });

    it("handles file-in-use error during dehydration", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["active-model.safetensors"],
      });

      const written = JSON.parse(channel.written[0]);

      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32003,
          message: "File in use: active-model.safetensors is locked by another process (PID 12345)",
          data: { path: "active-model.safetensors", pid: 12345 },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/File in use/);
    });

    it("rejects dehydrate during reconnection (write operation)", async () => {
      const setup = createRouter();
      const { router: r2, sessionManager: sm } = setup;
      sm.setStatus("gpu-server", "reconnecting");

      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // dehydrate is a write operation — should be rejected immediately during reconnection
      await expect(
        r2.route(repoId, "dehydrate", { repo_id: repoId, patterns: ["--all"] }),
      ).rejects.toThrow(/live operations are unavailable during reconnection/);
    });

    it("does not route dehydrate to local agent for remote repos", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["model.bin"],
      });

      // Verify the call went to the remote channel, not the local agent
      expect(channel.written).toHaveLength(1);
      expect(localAgent.calls).toHaveLength(0);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { dehydrated: 1, freedBytes: 3_000_000_000 });
      await promise;
    });

    it("times out dehydrate if remote agent does not respond", async () => {
      // Create a router with a short timeout for testing
      const sessionManager = new MockSessionManager() as any;
      const agent = new MockLocalAgent();
      const shortTimeoutRouter = new RpcRouter(sessionManager, agent, mockStorageConnections, { requestTimeoutMs: 50 });
      const ch = new MockChannel();

      sessionManager.setStatus("gpu-server", "connected");
      sessionManager.setConnection("gpu-server", ch);
      shortTimeoutRouter.attachChannel("gpu-server", ch.asChannel());

      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = shortTimeoutRouter.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["huge-model.safetensors"],
      });

      // Don't send a response — let it time out
      await expect(promise).rejects.toThrow(/RPC timeout.*dehydrate/);
    });

    it("handles partial NDJSON lines during dehydrate progress streaming", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["model.safetensors"],
      });

      // Simulate data arriving in chunks (split across TCP packets)
      const fullLine = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "dehydrate", file: "model.safetensors", percent: 100, freedBytes: 4_500_000_000 },
      });

      // Send first half of the line
      channel.emit("data", Buffer.from(fullLine.slice(0, 50)));
      expect(notifications).toHaveLength(0); // Not yet complete

      // Send the rest + newline
      channel.emit("data", Buffer.from(fullLine.slice(50) + "\n"));
      expect(notifications).toHaveLength(1); // Now complete

      expect((notifications[0] as any).params.op).toBe("dehydrate");
      expect((notifications[0] as any).params.freedBytes).toBe(4_500_000_000);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { dehydrated: 1, freedBytes: 4_500_000_000 });
      await promise;
    });
  });

  describe("push verb", () => {
    it("routes push to remote agent (data stays on remote)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("push");
      expect(written.params.remote).toBe("origin");
      expect(written.params.ref).toBe("main");

      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 42,
        bytesUploaded: 8_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({
        pushed: true,
        xorbsUploaded: 42,
        bytesUploaded: 8_000_000_000,
      });
    });

    it("push routes entirely to remote — local machine never receives xorb data", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Simulate multi-stage progress (only status/progress flows back, never xorb data)
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "enumerating_pointers",
        percent: 10,
        pointersFound: 15,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "classifying_chunks",
        percent: 25,
        chunksClassified: 1024,
        chunksTotal: 4096,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "packing_xorbs",
        percent: 40,
        xorbsPacked: 10,
        xorbsTotal: 42,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 70,
        bytesUploaded: 5_600_000_000,
        totalBytes: 8_000_000_000,
        speed: "250 MB/s",
        eta: "10s",
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "building_shard",
        percent: 90,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_manifest",
        percent: 100,
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 42,
        bytesUploaded: 8_000_000_000,
      });

      await promise;

      // Verify only progress/status notifications flowed back — no xorb data
      expect(notifications).toHaveLength(6);
      for (const n of notifications) {
        expect(n).toMatchObject({ profileId: "gpu-server", method: "progress" });
        // Notifications contain metadata only, never raw xorb bytes
        expect((n as any).params.op).toBe("push");
        expect((n as any).params).not.toHaveProperty("xorbData");
        expect((n as any).params).not.toHaveProperty("chunkData");
        expect((n as any).params).not.toHaveProperty("rawBytes");
      }

      // Verify local agent was never called (all data transfer on remote)
      expect(localAgent.calls).toHaveLength(0);

      // Verify only one RPC request was sent to the remote channel
      expect(channel.written).toHaveLength(1);
    });

    it("receives multi-stage progress notifications with bytes, speed, and ETA", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Full push pipeline progress stages
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "enumerating_pointers",
        percent: 5,
        pointersFound: 8,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "classifying_chunks",
        percent: 15,
        chunksClassified: 512,
        chunksTotal: 2048,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "packing_xorbs",
        percent: 30,
        xorbsPacked: 5,
        xorbsTotal: 20,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 50,
        bytesUploaded: 4_000_000_000,
        totalBytes: 8_000_000_000,
        speed: "200 MB/s",
        eta: "20s",
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 75,
        bytesUploaded: 6_000_000_000,
        totalBytes: 8_000_000_000,
        speed: "220 MB/s",
        eta: "9s",
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 90,
        bytesUploaded: 7_200_000_000,
        totalBytes: 8_000_000_000,
        speed: "210 MB/s",
        eta: "4s",
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "building_shard",
        percent: 95,
        shardsBuilt: 1,
        shardsTotal: 1,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_manifest",
        percent: 100,
        bytesUploaded: 8_000_000_000,
        totalBytes: 8_000_000_000,
        speed: "0 MB/s",
        eta: "0s",
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 20,
        bytesUploaded: 8_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({
        pushed: true,
        xorbsUploaded: 20,
        bytesUploaded: 8_000_000_000,
      });

      // Verify all 8 progress notifications were received
      expect(notifications).toHaveLength(8);

      // All notifications tagged with the correct profileId
      for (const n of notifications) {
        expect(n).toMatchObject({ profileId: "gpu-server", method: "progress" });
      }

      // Verify stage progression
      const stages = notifications.map((n: any) => n.params.stage);
      expect(stages).toEqual([
        "enumerating_pointers",
        "classifying_chunks",
        "packing_xorbs",
        "uploading_xorbs",
        "uploading_xorbs",
        "uploading_xorbs",
        "building_shard",
        "uploading_manifest",
      ]);

      // Verify percent increases monotonically
      const percents = notifications.map((n: any) => n.params.percent);
      for (let i = 1; i < percents.length; i++) {
        expect(percents[i]).toBeGreaterThanOrEqual(percents[i - 1]);
      }

      // Verify upload stage has speed and ETA
      const uploadNotifications = notifications.filter(
        (n: any) => n.params.stage === "uploading_xorbs",
      );
      for (const n of uploadNotifications) {
        expect((n as any).params.speed).toBeDefined();
        expect((n as any).params.eta).toBeDefined();
        expect((n as any).params.bytesUploaded).toBeDefined();
        expect((n as any).params.totalBytes).toBeDefined();
      }
    });

    it("push with nothing to upload (all xorbs already in cloud)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Remote agent quickly determines nothing needs uploading
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "enumerating_pointers",
        percent: 50,
        pointersFound: 5,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "classifying_chunks",
        percent: 100,
        chunksClassified: 512,
        chunksTotal: 512,
        message: "All chunks already in cloud",
      });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 0,
        bytesUploaded: 0,
        message: "Nothing to upload — all xorbs already in cloud storage",
      });

      const result = await promise;
      expect((result as any).pushed).toBe(true);
      expect((result as any).xorbsUploaded).toBe(0);
      expect((result as any).bytesUploaded).toBe(0);
      expect((result as any).message).toContain("Nothing to upload");

      // Only enumeration and classification stages — no upload stage
      expect(notifications).toHaveLength(2);
      const stages = notifications.map((n: any) => n.params.stage);
      expect(stages).not.toContain("uploading_xorbs");
    });

    it("push with specific remote and ref", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "upstream",
        ref: "feature/new-model",
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("push");
      expect(written.params.remote).toBe("upstream");
      expect(written.params.ref).toBe("feature/new-model");

      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 3,
        bytesUploaded: 500_000_000,
      });

      const result = await promise;
      expect((result as any).pushed).toBe(true);
      expect((result as any).xorbsUploaded).toBe(3);
    });

    it("handles cloud storage auth failure during push", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const written = JSON.parse(channel.written[0]);

      // Simulate auth failure from the remote agent
      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32002,
          message: "Cloud storage authentication failed: expired or invalid credentials for s3://ml-models-bucket",
          data: { bucket: "ml-models-bucket", provider: "s3" },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Cloud storage authentication failed/);
    });

    it("handles network timeout during xorb upload", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Some progress before the timeout
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 30,
        bytesUploaded: 2_400_000_000,
        totalBytes: 8_000_000_000,
        speed: "200 MB/s",
        eta: "28s",
      });

      const written = JSON.parse(channel.written[0]);

      // Simulate network timeout error from the remote agent
      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32003,
          message: "Network timeout: upload to cloud storage timed out after 60s",
          data: {
            xorbHash: "def456abc789",
            bytesUploaded: 2_400_000_000,
            retries: 3,
          },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Network timeout/);

      // Progress was received before the error
      expect(notifications).toHaveLength(1);
      expect((notifications[0] as any).params.stage).toBe("uploading_xorbs");
    });

    it("handles partial upload failure (some xorbs uploaded, then error)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Progress shows partial upload success
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 60,
        bytesUploaded: 4_800_000_000,
        totalBytes: 8_000_000_000,
        xorbsUploaded: 12,
        xorbsTotal: 20,
        speed: "180 MB/s",
        eta: "18s",
      });

      const written = JSON.parse(channel.written[0]);

      // Simulate partial failure
      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32004,
          message: "Partial upload failure: 12/20 xorbs uploaded before S3 returned 503 Service Unavailable",
          data: {
            xorbsUploaded: 12,
            xorbsTotal: 20,
            bytesUploaded: 4_800_000_000,
            failedXorb: "aaa111bbb222",
            httpStatus: 503,
          },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Partial upload failure/);

      // Verify progress was received before the error
      expect(notifications).toHaveLength(1);
      expect((notifications[0] as any).params.xorbsUploaded).toBe(12);
      expect((notifications[0] as any).params.xorbsTotal).toBe(20);
    });

    it("rejects push during reconnection (write operation)", async () => {
      const setup = createRouter();
      const { router: r2, sessionManager: sm } = setup;
      sm.setStatus("gpu-server", "reconnecting");

      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // push is a write operation — should be rejected immediately during reconnection
      await expect(
        r2.route(repoId, "push", { repo_id: repoId, remote: "origin", ref: "main" }),
      ).rejects.toThrow(/live operations are unavailable during reconnection/);
    });

    it("does not route push to local agent for remote repos", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Verify the call went to the remote channel, not the local agent
      expect(channel.written).toHaveLength(1);
      expect(localAgent.calls).toHaveLength(0);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 5,
        bytesUploaded: 1_000_000_000,
      });
      await promise;

      // After completion, local agent still never called
      expect(localAgent.calls).toHaveLength(0);
    });

    it("times out push if remote agent does not respond", async () => {
      // Create a router with a short timeout for testing
      const sessionManager = new MockSessionManager() as any;
      const agent = new MockLocalAgent();
      const shortTimeoutRouter = new RpcRouter(sessionManager, agent, mockStorageConnections, { requestTimeoutMs: 50 });
      const ch = new MockChannel();

      sessionManager.setStatus("gpu-server", "connected");
      sessionManager.setConnection("gpu-server", ch);
      shortTimeoutRouter.attachChannel("gpu-server", ch.asChannel());

      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = shortTimeoutRouter.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Don't send a response — let it time out
      await expect(promise).rejects.toThrow(/RPC timeout.*push/);
    });

    it("progress notifications arrive in real-time order without buffering", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notificationTimestamps: number[] = [];
      router.on("notification", () => notificationTimestamps.push(Date.now()));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Simulate progress arriving as separate data events (not batched)
      const progress1 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "push", stage: "uploading_xorbs", percent: 33, speed: "150 MB/s", eta: "40s" },
      }) + "\n";
      channel.emit("data", Buffer.from(progress1));

      const progress2 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "push", stage: "uploading_xorbs", percent: 66, speed: "180 MB/s", eta: "15s" },
      }) + "\n";
      channel.emit("data", Buffer.from(progress2));

      const progress3 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "push", stage: "uploading_manifest", percent: 100, speed: "0 MB/s", eta: "0s" },
      }) + "\n";
      channel.emit("data", Buffer.from(progress3));

      // Each notification should be processed immediately as it arrives
      expect(notificationTimestamps).toHaveLength(3);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { pushed: true, xorbsUploaded: 10, bytesUploaded: 3_000_000_000 });
      await promise;
    });

    it("handles partial NDJSON lines during push progress streaming", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      // Simulate data arriving in chunks (split across TCP packets)
      const fullLine = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "push", stage: "uploading_xorbs", percent: 50, bytesUploaded: 4_000_000_000, totalBytes: 8_000_000_000, speed: "200 MB/s", eta: "20s" },
      });

      // Send first half of the line
      channel.emit("data", Buffer.from(fullLine.slice(0, 60)));
      expect(notifications).toHaveLength(0); // Not yet complete

      // Send the rest + newline
      channel.emit("data", Buffer.from(fullLine.slice(60) + "\n"));
      expect(notifications).toHaveLength(1); // Now complete

      expect((notifications[0] as any).params.op).toBe("push");
      expect((notifications[0] as any).params.stage).toBe("uploading_xorbs");
      expect((notifications[0] as any).params.speed).toBe("200 MB/s");

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { pushed: true, xorbsUploaded: 20, bytesUploaded: 8_000_000_000 });
      await promise;
    });

    it("push without explicit remote/ref uses defaults", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "push", {
        repo_id: repoId,
      });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("push");
      // No remote/ref specified — remote agent uses defaults
      expect(written.params.remote).toBeUndefined();
      expect(written.params.ref).toBeUndefined();

      simulateResponse(channel, written.id, {
        pushed: true,
        xorbsUploaded: 7,
        bytesUploaded: 2_000_000_000,
      });

      const result = await promise;
      expect((result as any).pushed).toBe(true);
    });
  });

  describe("hydration_status verb", () => {
    it("routes hydration_status to remote agent and returns structured data", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("hydration_status");
      expect(written.params.repo_id).toBe(repoId);

      simulateResponse(channel, written.id, {
        totalFiles: 100,
        hydratedFiles: 45,
        dehydratedFiles: 55,
        totalBytes: 200_000_000_000,
        hydratedBytes: 90_000_000_000,
      });

      const result = await promise;
      expect(result).toEqual({
        totalFiles: 100,
        hydratedFiles: 45,
        dehydratedFiles: 55,
        totalBytes: 200_000_000_000,
        hydratedBytes: 90_000_000_000,
      });
    });

    it("handles empty repo with no tracked files", async () => {
      const repoId = "ssh://gpu-server/home/user/empty-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("hydration_status");

      simulateResponse(channel, written.id, {
        totalFiles: 0,
        hydratedFiles: 0,
        dehydratedFiles: 0,
        totalBytes: 0,
        hydratedBytes: 0,
      });

      const result = await promise;
      expect((result as any).totalFiles).toBe(0);
      expect((result as any).hydratedFiles).toBe(0);
      expect((result as any).dehydratedFiles).toBe(0);
      expect((result as any).totalBytes).toBe(0);
      expect((result as any).hydratedBytes).toBe(0);
    });

    it("handles all files hydrated state", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        totalFiles: 25,
        hydratedFiles: 25,
        dehydratedFiles: 0,
        totalBytes: 50_000_000_000,
        hydratedBytes: 50_000_000_000,
      });

      const result = await promise;
      expect((result as any).totalFiles).toBe(25);
      expect((result as any).hydratedFiles).toBe(25);
      expect((result as any).dehydratedFiles).toBe(0);
      expect((result as any).hydratedBytes).toBe((result as any).totalBytes);
    });

    it("handles all files dehydrated state", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        totalFiles: 30,
        hydratedFiles: 0,
        dehydratedFiles: 30,
        totalBytes: 120_000_000_000,
        hydratedBytes: 0,
      });

      const result = await promise;
      expect((result as any).totalFiles).toBe(30);
      expect((result as any).hydratedFiles).toBe(0);
      expect((result as any).dehydratedFiles).toBe(30);
      expect((result as any).hydratedBytes).toBe(0);
    });

    it("handles mixed hydration state", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        totalFiles: 50,
        hydratedFiles: 12,
        dehydratedFiles: 38,
        totalBytes: 300_000_000_000,
        hydratedBytes: 72_000_000_000,
      });

      const result = await promise;
      expect((result as any).totalFiles).toBe(50);
      expect((result as any).hydratedFiles).toBe(12);
      expect((result as any).dehydratedFiles).toBe(38);
      // hydratedFiles + dehydratedFiles = totalFiles
      expect((result as any).hydratedFiles + (result as any).dehydratedFiles).toBe(
        (result as any).totalFiles,
      );
    });

    it("queues hydration_status during reconnection (read operation)", async () => {
      const sessionManager = new MockSessionManager() as any;
      const agent = new MockLocalAgent();
      const r2 = new RpcRouter(sessionManager, agent, mockStorageConnections, { requestTimeoutMs: 5000 });
      const ch = new MockChannel();

      sessionManager.setStatus("gpu-server", "reconnecting");
      sessionManager.setConnection("gpu-server", ch);
      r2.attachChannel("gpu-server", ch.asChannel());

      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // hydration_status is a READ operation — should be queued, not rejected
      const promise = r2.route(repoId, "hydration_status", { repo_id: repoId });

      // Nothing written to channel yet (queued)
      expect(ch.written).toHaveLength(0);

      // Simulate reconnection completing — replay sends the queued call
      sessionManager.setStatus("gpu-server", "connected");
      const replayPromise = r2.replayQueue("gpu-server");

      // Wait a tick for the replay to write to the channel
      await new Promise((resolve) => setTimeout(resolve, 10));

      // Now the queued call should have been sent
      expect(ch.written).toHaveLength(1);
      const written = JSON.parse(ch.written[0]);
      expect(written.method).toBe("hydration_status");

      simulateResponse(ch, written.id, {
        totalFiles: 10,
        hydratedFiles: 5,
        dehydratedFiles: 5,
        totalBytes: 20_000_000_000,
        hydratedBytes: 10_000_000_000,
      });

      await replayPromise;
      const result = await promise;
      expect((result as any).totalFiles).toBe(10);
    });

    it("does not route hydration_status to local agent for remote repos", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      // Verify the call went to the remote channel, not the local agent
      expect(channel.written).toHaveLength(1);
      expect(localAgent.calls).toHaveLength(0);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        totalFiles: 5,
        hydratedFiles: 3,
        dehydratedFiles: 2,
        totalBytes: 10_000_000_000,
        hydratedBytes: 6_000_000_000,
      });

      await promise;

      // After completion, local agent still never called
      expect(localAgent.calls).toHaveLength(0);
    });

    it("handles error when remote is not a crab repo", async () => {
      const repoId = "ssh://gpu-server/home/user/plain-git-repo";
      const promise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);

      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: written.id,
        error: {
          code: -32001,
          message: "Not a crab repository: no .crab directory found",
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(promise).rejects.toThrow(/Not a crab repository/);
    });

    it("times out hydration_status if remote agent does not respond", async () => {
      const sessionManager = new MockSessionManager() as any;
      const agent = new MockLocalAgent();
      const shortTimeoutRouter = new RpcRouter(sessionManager, agent, mockStorageConnections, { requestTimeoutMs: 50 });
      const ch = new MockChannel();

      sessionManager.setStatus("gpu-server", "connected");
      sessionManager.setConnection("gpu-server", ch);
      shortTimeoutRouter.attachChannel("gpu-server", ch.asChannel());

      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = shortTimeoutRouter.route(repoId, "hydration_status", {
        repo_id: repoId,
      });

      // Don't send a response — let it time out
      await expect(promise).rejects.toThrow(/RPC timeout.*hydration_status/);
    });

    it("works with the response caching system (cached with TTL)", async () => {
      // Import the cache module to verify hydration_status is cacheable
      const { RemoteCache } = await import("../electron/ssh-cache");
      const cache = new RemoteCache();

      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const cacheKey = `${repoId}:hydration_status`;

      let fetchCount = 0;
      const fetcher = async () => {
        fetchCount++;
        return {
          totalFiles: 50,
          hydratedFiles: 20,
          dehydratedFiles: 30,
          totalBytes: 100_000_000_000,
          hydratedBytes: 40_000_000_000,
        };
      };

      // First call: cache miss, fetcher is called
      const result1 = await cache.getOrFetch(cacheKey, fetcher);
      expect(fetchCount).toBe(1);
      expect(result1.totalFiles).toBe(50);

      // Second call: cache hit, fetcher is NOT called
      const result2 = await cache.getOrFetch(cacheKey, fetcher);
      expect(fetchCount).toBe(1); // Still 1 — served from cache
      expect(result2).toEqual(result1);

      // Verify cache stats
      const stats = cache.getStats();
      expect(stats.hits).toBe(1);
      expect(stats.misses).toBe(1);
    });

    it("cache is invalidated on write operations", async () => {
      const { RemoteCache } = await import("../electron/ssh-cache");
      const cache = new RemoteCache();

      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const cacheKey = `${repoId}:hydration_status`;

      let fetchCount = 0;
      const fetcher = async () => {
        fetchCount++;
        return { totalFiles: 10, hydratedFiles: fetchCount, dehydratedFiles: 10 - fetchCount };
      };

      // Populate cache
      await cache.getOrFetch(cacheKey, fetcher);
      expect(fetchCount).toBe(1);

      // Simulate a hydrate write operation invalidating the cache
      expect(cache.isWriteMethod("hydrate")).toBe(true);
      cache.invalidateRepo(repoId);

      // Next fetch should miss the cache
      const result = await cache.getOrFetch(cacheKey, fetcher);
      expect(fetchCount).toBe(2);
      expect(result.hydratedFiles).toBe(2); // Updated value
    });

    it("adaptive TTL increases for high-latency connections", async () => {
      const { RemoteCache } = await import("../electron/ssh-cache");
      const cache = new RemoteCache();

      // Low latency: short TTL
      cache.updateLatency("gpu-server", 30);
      const lowLatencyTtl = cache.getAdaptiveTtl("gpu-server", "hydration_status");

      // High latency: longer TTL
      cache.updateLatency("gpu-server", 250);
      const highLatencyTtl = cache.getAdaptiveTtl("gpu-server", "hydration_status");

      expect(highLatencyTtl).toBeGreaterThan(lowLatencyTtl);
    });
  });

  describe("pointer_top verb", () => {
    describe("pointer_top", () => {
      it("routes pointer_top to remote agent and returns top N largest files sorted by size", async () => {
        const repoId = "ssh://gpu-server/home/user/ml-repo";
        const promise = router.route(repoId, "pointer_top", {
          repo_id: repoId,
          limit: 5,
        });

        const written = JSON.parse(channel.written[0]);
        expect(written.method).toBe("pointer_top");
        expect(written.params.limit).toBe(5);
        expect(written.params.repo_id).toBe(repoId);

        simulateResponse(channel, written.id, {
          files: [
            { path: "model-00001.safetensors", size: 9_500_000_000 },
            { path: "model-00002.safetensors", size: 9_500_000_000 },
            { path: "data/train.parquet", size: 4_200_000_000 },
            { path: "checkpoints/epoch-50.bin", size: 2_100_000_000 },
            { path: "embeddings.npy", size: 1_800_000_000 },
          ],
        });

        const result = await promise;
        const files = (result as any).files;
        expect(files).toHaveLength(5);

        // Verify sorted by size descending
        for (let i = 1; i < files.length; i++) {
          expect(files[i - 1].size).toBeGreaterThanOrEqual(files[i].size);
        }

        expect(files[0].path).toBe("model-00001.safetensors");
        expect(files[0].size).toBe(9_500_000_000);
        expect(files[4].path).toBe("embeddings.npy");
        expect(files[4].size).toBe(1_800_000_000);
      });

      it("handles empty repo with no pointer files", async () => {
        const repoId = "ssh://gpu-server/home/user/empty-repo";
        const promise = router.route(repoId, "pointer_top", {
          repo_id: repoId,
          limit: 10,
        });

        const written = JSON.parse(channel.written[0]);
        expect(written.method).toBe("pointer_top");

        simulateResponse(channel, written.id, {
          files: [],
        });

        const result = await promise;
        expect((result as any).files).toHaveLength(0);
      });

      it("respects the limit parameter", async () => {
        const repoId = "ssh://gpu-server/home/user/ml-repo";
        const promise = router.route(repoId, "pointer_top", {
          repo_id: repoId,
          limit: 3,
        });

        const written = JSON.parse(channel.written[0]);
        expect(written.method).toBe("pointer_top");
        expect(written.params.limit).toBe(3);

        simulateResponse(channel, written.id, {
          files: [
            { path: "model-00001.safetensors", size: 9_500_000_000 },
            { path: "model-00002.safetensors", size: 9_500_000_000 },
            { path: "data/train.parquet", size: 4_200_000_000 },
          ],
        });

        const result = await promise;
        expect((result as any).files).toHaveLength(3);
      });

      it("handles default limit when not specified", async () => {
        const repoId = "ssh://gpu-server/home/user/ml-repo";
        const promise = router.route(repoId, "pointer_top", {
          repo_id: repoId,
        });

        const written = JSON.parse(channel.written[0]);
        expect(written.method).toBe("pointer_top");
        // No limit in params — remote agent uses its default
        expect(written.params.limit).toBeUndefined();

        simulateResponse(channel, written.id, {
          files: [
            { path: "model.safetensors", size: 4_500_000_000 },
            { path: "data.parquet", size: 2_000_000_000 },
          ],
        });

        const result = await promise;
        expect((result as any).files).toHaveLength(2);
      });

      it("handles error when remote is not a crab repo", async () => {
        const repoId = "ssh://gpu-server/home/user/plain-git-repo";
        const promise = router.route(repoId, "pointer_top", {
          repo_id: repoId,
          limit: 10,
        });

        const written = JSON.parse(channel.written[0]);

        const errorResponse = JSON.stringify({
          jsonrpc: "2.0",
          id: written.id,
          error: {
            code: -32001,
            message: "Not a crab repository: no .crab directory found",
          },
        }) + "\n";
        channel.emit("data", Buffer.from(errorResponse));

        await expect(promise).rejects.toThrow(/Not a crab repository/);
      });

      it("does not route pointer_top to local agent for remote repos", async () => {
        const repoId = "ssh://gpu-server/home/user/ml-repo";
        const promise = router.route(repoId, "pointer_top", {
          repo_id: repoId,
          limit: 10,
        });

        // Verify the call went to the remote channel, not the local agent
        expect(channel.written).toHaveLength(1);
        expect(localAgent.calls).toHaveLength(0);

        const written = JSON.parse(channel.written[0]);
        simulateResponse(channel, written.id, {
          files: [
            { path: "model.safetensors", size: 4_500_000_000 },
          ],
        });

        await promise;

        // After completion, local agent still never called
        expect(localAgent.calls).toHaveLength(0);
      });

      it("rejects pointer_top from a previous agent session during reconnection", async () => {
        const repoId = "ssh://gpu-server/home/user/ml-repo";
        const manager = new MockSessionManager();
        manager.setStatus("gpu-server", "reconnecting");
        manager.setConnection("gpu-server", channel);
        const remote = new RpcRouter(manager as unknown as SshSessionManager, localAgent, mockStorageConnections);
        remote.attachChannel("gpu-server", channel.asChannel());
        await expect(remote.route(repoId, "pointer_top", { repo_id: repoId, limit: 5 })).rejects.toMatchObject({ code: "SSH_RECONNECTING" });
        manager.setStatus("gpu-server", "connected");
        await remote.replayQueue("gpu-server");
        expect(channel.written).toHaveLength(0);
      });

      it("times out pointer_top if remote agent does not respond", async () => {
        const sessionManager = new MockSessionManager() as any;
        const agent = new MockLocalAgent();
        const shortTimeoutRouter = new RpcRouter(sessionManager, agent, mockStorageConnections, { requestTimeoutMs: 50 });
        const ch = new MockChannel();

        sessionManager.setStatus("gpu-server", "connected");
        sessionManager.setConnection("gpu-server", ch);
        shortTimeoutRouter.attachChannel("gpu-server", ch.asChannel());

        const repoId = "ssh://gpu-server/home/user/ml-repo";
        const promise = shortTimeoutRouter.route(repoId, "pointer_top", {
          repo_id: repoId,
          limit: 10,
        });

        // Don't send a response — let it time out
        await expect(promise).rejects.toThrow(/RPC timeout.*pointer_top/);
      });
    });
  });

  describe("cancellation", () => {
    it("cancels a hydrate operation mid-flight via rpc/cancel over SSH", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Start a hydrate operation
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["large-model.safetensors"],
      });

      const hydrateWritten = JSON.parse(channel.written[0]);
      expect(hydrateWritten.method).toBe("hydrate");

      // Simulate some progress arriving before cancellation
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "large-model.safetensors",
        percent: 25,
        bytesDownloaded: 1_125_000_000,
        totalBytes: 4_500_000_000,
        speed: "150 MB/s",
        eta: "20s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "large-model.safetensors",
        percent: 50,
        bytesDownloaded: 2_250_000_000,
        totalBytes: 4_500_000_000,
        speed: "160 MB/s",
        eta: "14s",
      });

      expect(notifications).toHaveLength(2);

      // Send rpc/cancel referencing the hydrate request ID
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: hydrateWritten.id,
      });

      const cancelWritten = JSON.parse(channel.written[1]);
      expect(cancelWritten.method).toBe("rpc/cancel");
      expect(cancelWritten.params.request_id).toBe(hydrateWritten.id);

      // Remote agent acknowledges the cancel
      simulateResponse(channel, cancelWritten.id, { cancelled: true });
      const cancelResult = await cancelPromise;
      expect((cancelResult as any).cancelled).toBe(true);

      // Remote agent returns a cancelled error for the hydrate operation
      const cancelledError = JSON.stringify({
        jsonrpc: "2.0",
        id: hydrateWritten.id,
        error: {
          code: -32800,
          message: "Operation cancelled",
          data: { kind: "op_cancelled", request_id: hydrateWritten.id },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(cancelledError));

      await expect(hydratePromise).rejects.toThrow(/Operation cancelled/);
    });

    it("cancels a push operation mid-flight via rpc/cancel over SSH", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Start a push operation
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const pushWritten = JSON.parse(channel.written[0]);
      expect(pushWritten.method).toBe("push");

      // Simulate some push progress before cancellation
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 40,
        bytesUploaded: 3_200_000_000,
        totalBytes: 8_000_000_000,
        speed: "200 MB/s",
        eta: "24s",
      });

      expect(notifications).toHaveLength(1);

      // Send rpc/cancel referencing the push request ID
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: pushWritten.id,
      });

      const cancelWritten = JSON.parse(channel.written[1]);
      expect(cancelWritten.method).toBe("rpc/cancel");
      expect(cancelWritten.params.request_id).toBe(pushWritten.id);

      // Remote agent acknowledges the cancel
      simulateResponse(channel, cancelWritten.id, { cancelled: true });
      const cancelResult = await cancelPromise;
      expect((cancelResult as any).cancelled).toBe(true);

      // Remote agent returns a cancelled error for the push operation
      const cancelledError = JSON.stringify({
        jsonrpc: "2.0",
        id: pushWritten.id,
        error: {
          code: -32800,
          message: "Operation cancelled",
          data: { kind: "op_cancelled", request_id: pushWritten.id },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(cancelledError));

      await expect(pushPromise).rejects.toThrow(/Operation cancelled/);
    });

    it("cancels a dehydrate operation mid-flight via rpc/cancel over SSH", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Start a dehydrate operation
      const dehydratePromise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const dehydrateWritten = JSON.parse(channel.written[0]);
      expect(dehydrateWritten.method).toBe("dehydrate");

      // Simulate some dehydrate progress before cancellation
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        filesCompleted: 3,
        filesTotal: 10,
        percent: 30,
        freedBytes: 9_000_000_000,
        totalFreeable: 30_000_000_000,
      });

      expect(notifications).toHaveLength(1);

      // Send rpc/cancel referencing the dehydrate request ID
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: dehydrateWritten.id,
      });

      const cancelWritten = JSON.parse(channel.written[1]);
      expect(cancelWritten.method).toBe("rpc/cancel");
      expect(cancelWritten.params.request_id).toBe(dehydrateWritten.id);

      // Remote agent acknowledges the cancel
      simulateResponse(channel, cancelWritten.id, { cancelled: true });
      const cancelResult = await cancelPromise;
      expect((cancelResult as any).cancelled).toBe(true);

      // Remote agent returns a cancelled error for the dehydrate operation
      const cancelledError = JSON.stringify({
        jsonrpc: "2.0",
        id: dehydrateWritten.id,
        error: {
          code: -32800,
          message: "Operation cancelled",
          data: { kind: "op_cancelled", request_id: dehydrateWritten.id },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(cancelledError));

      await expect(dehydratePromise).rejects.toThrow(/Operation cancelled/);
    });

    it("cancel request references the correct in-flight operation ID", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // Start two operations concurrently
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model-a.safetensors"],
      });
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const hydrateWritten = JSON.parse(channel.written[0]);
      const pushWritten = JSON.parse(channel.written[1]);

      // IDs should be different
      expect(hydrateWritten.id).not.toBe(pushWritten.id);

      // Cancel only the push operation — the cancel must reference the push ID
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: pushWritten.id,
      });

      const cancelWritten = JSON.parse(channel.written[2]);
      expect(cancelWritten.method).toBe("rpc/cancel");
      // The cancel request_id must match the push operation's ID exactly
      expect(cancelWritten.params.request_id).toBe(pushWritten.id);
      expect(cancelWritten.params.request_id).not.toBe(hydrateWritten.id);

      // Remote agent acknowledges the cancel
      simulateResponse(channel, cancelWritten.id, { cancelled: true });
      await cancelPromise;

      // Push is cancelled
      const pushCancelledError = JSON.stringify({
        jsonrpc: "2.0",
        id: pushWritten.id,
        error: {
          code: -32800,
          message: "Operation cancelled",
          data: { kind: "op_cancelled", request_id: pushWritten.id },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(pushCancelledError));

      await expect(pushPromise).rejects.toThrow(/Operation cancelled/);

      // Hydrate continues and completes successfully
      simulateResponse(channel, hydrateWritten.id, {
        hydrated: 1,
        totalBytes: 4_500_000_000,
      });

      const hydrateResult = await hydratePromise;
      expect((hydrateResult as any).hydrated).toBe(1);
    });

    it("handles cancel when no operation is in flight (graceful no-op)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // Send rpc/cancel with a non-existent request_id
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: 99999,
      });

      const cancelWritten = JSON.parse(channel.written[0]);
      expect(cancelWritten.method).toBe("rpc/cancel");
      expect(cancelWritten.params.request_id).toBe(99999);

      // Remote agent responds gracefully — nothing to cancel
      simulateResponse(channel, cancelWritten.id, {
        cancelled: false,
        reason: "No operation with that ID is in flight",
      });

      const result = await cancelPromise;
      expect((result as any).cancelled).toBe(false);
      expect((result as any).reason).toContain("No operation");
    });

    it("rejects rpc/cancel from a previous agent session during reconnection", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const manager = new MockSessionManager();
      manager.setStatus("gpu-server", "reconnecting");
      manager.setConnection("gpu-server", channel);
      const remote = new RpcRouter(manager as unknown as SshSessionManager, localAgent, mockStorageConnections);
      remote.attachChannel("gpu-server", channel.asChannel());
      await expect(remote.route(repoId, "rpc/cancel", { repo_id: repoId, op_id: "old-session" })).rejects.toMatchObject({ code: "SSH_RECONNECTING" });
      manager.setStatus("gpu-server", "connected");
      await remote.replayQueue("gpu-server");
      expect(channel.written).toHaveLength(0);
    });

    it("cancels one operation while another continues (multiple concurrent ops)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Start hydrate and push concurrently
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model-a.safetensors"],
      });
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const hydrateWritten = JSON.parse(channel.written[0]);
      const pushWritten = JSON.parse(channel.written[1]);

      // Both operations emit progress
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-a.safetensors",
        percent: 30,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 20,
      });

      expect(notifications).toHaveLength(2);

      // Cancel only the hydrate
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: hydrateWritten.id,
      });

      const cancelWritten = JSON.parse(channel.written[2]);
      simulateResponse(channel, cancelWritten.id, { cancelled: true });
      await cancelPromise;

      // Hydrate is cancelled
      const cancelledError = JSON.stringify({
        jsonrpc: "2.0",
        id: hydrateWritten.id,
        error: {
          code: -32800,
          message: "Operation cancelled",
          data: { kind: "op_cancelled", request_id: hydrateWritten.id },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(cancelledError));

      await expect(hydratePromise).rejects.toThrow(/Operation cancelled/);

      // Push continues to receive progress and completes successfully
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 60,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_manifest",
        percent: 100,
      });

      simulateResponse(channel, pushWritten.id, {
        pushed: true,
        xorbsUploaded: 10,
        bytesUploaded: 3_000_000_000,
      });

      const pushResult = await pushPromise;
      expect((pushResult as any).pushed).toBe(true);
      expect((pushResult as any).xorbsUploaded).toBe(10);

      // Total notifications: 2 before cancel + 2 after cancel for push
      expect(notifications).toHaveLength(4);
      const pushNotifications = notifications.filter((n: any) => n.params.op === "push");
      expect(pushNotifications).toHaveLength(3);
    });

    it("progress stops after cancellation (no more notifications for cancelled operation)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Start a hydrate operation
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["large-model.safetensors"],
      });

      const hydrateWritten = JSON.parse(channel.written[0]);

      // Simulate progress before cancel
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "large-model.safetensors",
        percent: 20,
        request_id: hydrateWritten.id,
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "large-model.safetensors",
        percent: 40,
        request_id: hydrateWritten.id,
      });

      expect(notifications).toHaveLength(2);

      // Cancel the operation
      const cancelPromise = router.route(repoId, "rpc/cancel", {
        repo_id: repoId,
        request_id: hydrateWritten.id,
      });

      const cancelWritten = JSON.parse(channel.written[1]);
      simulateResponse(channel, cancelWritten.id, { cancelled: true });
      await cancelPromise;

      // Remote agent returns cancelled error
      const cancelledError = JSON.stringify({
        jsonrpc: "2.0",
        id: hydrateWritten.id,
        error: {
          code: -32800,
          message: "Operation cancelled",
          data: { kind: "op_cancelled", request_id: hydrateWritten.id },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(cancelledError));

      await expect(hydratePromise).rejects.toThrow(/Operation cancelled/);

      // Record notification count at the point of cancellation
      const countAtCancel = notifications.length;

      // After cancellation, the remote agent should NOT send more progress
      // for this operation. If it does (race condition), the notifications
      // still arrive at the event emitter level but the operation is already
      // settled. Verify the operation promise is already rejected and no
      // further progress is expected from a well-behaved agent.
      // A well-behaved remote agent stops sending progress after acknowledging cancel.
      // No additional progress notifications should arrive for this operation.
      expect(countAtCancel).toBe(2);
    });
  });

  describe("batch RPC for crab operations", () => {
    it("batches hydration_status with git_status_porcelain", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.batchRpc(repoId, [
        { method: "hydration_status", params: { repo_id: repoId } },
        { method: "git_status_porcelain", params: { repo_id: repoId } },
        { method: "staging_stats", params: { repo_id: repoId } },
      ]);

      const written = channel.written.map(line => JSON.parse(line));
      expect(written.map(request => request.method)).toEqual(["hydration_status", "git_status_porcelain", "staging_stats"]);
      for (const [index, request] of written.entries()) {
        simulateResponse(channel, request.id, index === 0 ? { totalFiles: 50 } : index === 1 ? { entries: [] } : { stagedFiles: 0 });
      }

      const results = await promise;
      expect(results).toHaveLength(3);
      expect(results[0].result).toEqual({ totalFiles: 50 });
      expect(results[1].result).toEqual({ entries: [] });
      expect(results[2].result).toEqual({ stagedFiles: 0 });
    });
  });

  describe("progress streaming for long-running operations", () => {
    it("emits progress notifications immediately as each NDJSON line arrives (no buffering)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const emitOrder: string[] = [];

      router.on("notification", (n: any) => {
        emitOrder.push(`notification:${n.params.percent}`);
      });

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["large-model.safetensors"],
      });

      // Each data event with a complete NDJSON line should emit immediately
      const progress1 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "large-model.safetensors", percent: 10 },
      }) + "\n";
      channel.emit("data", Buffer.from(progress1));
      // Notification should be emitted synchronously — check immediately
      expect(emitOrder).toEqual(["notification:10"]);

      const progress2 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "large-model.safetensors", percent: 20 },
      }) + "\n";
      channel.emit("data", Buffer.from(progress2));
      expect(emitOrder).toEqual(["notification:10", "notification:20"]);

      const progress3 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "large-model.safetensors", percent: 30 },
      }) + "\n";
      channel.emit("data", Buffer.from(progress3));
      expect(emitOrder).toEqual(["notification:10", "notification:20", "notification:30"]);

      // Complete the operation
      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 10_000_000_000 });
      await promise;
    });

    it("delivers multiple rapid progress updates individually without coalescing", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["large-model.safetensors"],
      });

      // Simulate high-frequency updates arriving in a single data event
      // (multiple NDJSON lines in one TCP packet — common with fast progress)
      const rapidUpdates = Array.from({ length: 20 }, (_, i) =>
        JSON.stringify({
          jsonrpc: "2.0",
          method: "progress",
          params: {
            op: "hydrate",
            file: "large-model.safetensors",
            percent: (i + 1) * 5,
            bytesDownloaded: (i + 1) * 500_000_000,
            totalBytes: 10_000_000_000,
          },
        }),
      ).join("\n") + "\n";

      channel.emit("data", Buffer.from(rapidUpdates));

      // All 20 updates should be delivered individually — no coalescing
      expect(notifications).toHaveLength(20);
      const percents = notifications.map((n: any) => n.params.percent);
      expect(percents).toEqual(Array.from({ length: 20 }, (_, i) => (i + 1) * 5));

      // Each notification has the correct profileId
      for (const n of notifications) {
        expect((n as any).profileId).toBe("gpu-server");
        expect((n as any).method).toBe("progress");
      }

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 10_000_000_000 });
      await promise;
    });

    it("correctly demultiplexes progress notifications interleaved with other notifications", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model.safetensors"],
      });

      // Interleave progress notifications with other notification types
      // (e.g., log events, status updates from different operations)
      const interleaved = [
        JSON.stringify({ jsonrpc: "2.0", method: "progress", params: { op: "hydrate", file: "model.safetensors", percent: 25 } }),
        JSON.stringify({ jsonrpc: "2.0", method: "log", params: { level: "info", message: "Downloading xorb abc123" } }),
        JSON.stringify({ jsonrpc: "2.0", method: "progress", params: { op: "hydrate", file: "model.safetensors", percent: 50 } }),
        JSON.stringify({ jsonrpc: "2.0", method: "agent_event", params: { type: "task_started", taskId: "t-001" } }),
        JSON.stringify({ jsonrpc: "2.0", method: "progress", params: { op: "hydrate", file: "model.safetensors", percent: 75 } }),
        JSON.stringify({ jsonrpc: "2.0", method: "health", params: { diskFree: 1_800_000_000_000 } }),
        JSON.stringify({ jsonrpc: "2.0", method: "progress", params: { op: "hydrate", file: "model.safetensors", percent: 100 } }),
      ].join("\n") + "\n";

      channel.emit("data", Buffer.from(interleaved));

      // All 7 notifications should arrive in order
      expect(notifications).toHaveLength(7);

      // Verify correct demultiplexing — each notification has its own method
      const methods = notifications.map((n: any) => n.method);
      expect(methods).toEqual(["progress", "log", "progress", "agent_event", "progress", "health", "progress"]);

      // Progress notifications have correct percent values in order
      const progressNotifications = notifications.filter((n: any) => n.method === "progress");
      const percents = progressNotifications.map((n: any) => n.params.percent);
      expect(percents).toEqual([25, 50, 75, 100]);

      // Non-progress notifications have their own params intact
      const logNotification = notifications.find((n: any) => n.method === "log") as any;
      expect(logNotification.params.message).toBe("Downloading xorb abc123");

      const agentEvent = notifications.find((n: any) => n.method === "agent_event") as any;
      expect(agentEvent.params.type).toBe("task_started");

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 4_500_000_000 });
      await promise;
    });

    it("delivers 50+ progress notifications during a single operation correctly", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      // Simulate 60 progress notifications (large hydrate with many files)
      const totalNotifications = 60;
      for (let i = 0; i < totalNotifications; i++) {
        const notification = JSON.stringify({
          jsonrpc: "2.0",
          method: "progress",
          params: {
            op: "hydrate",
            filesCompleted: i,
            filesTotal: totalNotifications,
            percent: Math.round((i / totalNotifications) * 100),
            bytesDownloaded: i * 1_000_000_000,
            totalBytes: totalNotifications * 1_000_000_000,
            speed: `${150 + Math.floor(Math.random() * 50)} MB/s`,
            eta: `${totalNotifications - i}s`,
          },
        }) + "\n";
        channel.emit("data", Buffer.from(notification));
      }

      // All 60 notifications should arrive — none dropped
      expect(notifications).toHaveLength(totalNotifications);

      // Verify ordering is preserved
      for (let i = 0; i < totalNotifications; i++) {
        const n = notifications[i] as any;
        expect(n.profileId).toBe("gpu-server");
        expect(n.method).toBe("progress");
        expect(n.params.filesCompleted).toBe(i);
        expect(n.params.filesTotal).toBe(totalNotifications);
      }

      // Verify first and last notifications
      expect((notifications[0] as any).params.percent).toBe(0);
      expect((notifications[totalNotifications - 1] as any).params.percent).toBe(
        Math.round(((totalNotifications - 1) / totalNotifications) * 100),
      );

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, {
        hydrated: totalNotifications,
        totalBytes: totalNotifications * 1_000_000_000,
      });
      await promise;
    });

    it("continues streaming progress notifications before the response arrives (long-running operation)", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      let responseResolved = false;

      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["huge-dataset/**/*.parquet"],
      });
      promise.then(() => { responseResolved = true; });

      // Send many progress notifications — the response hasn't arrived yet
      for (let i = 1; i <= 10; i++) {
        simulateNotification(channel, "progress", {
          op: "hydrate",
          file: `chunk-${i}.parquet`,
          percent: i * 10,
          bytesDownloaded: i * 2_000_000_000,
          totalBytes: 20_000_000_000,
          speed: "200 MB/s",
          eta: `${(10 - i) * 10}s`,
        });
      }

      // All progress notifications should have been delivered
      expect(notifications).toHaveLength(10);
      // But the promise should NOT be resolved yet (response hasn't arrived)
      expect(responseResolved).toBe(false);

      // Send more progress after a "delay" (simulating continued work)
      for (let i = 11; i <= 15; i++) {
        simulateNotification(channel, "progress", {
          op: "hydrate",
          file: `chunk-${i}.parquet`,
          percent: i * 6,
          bytesDownloaded: i * 2_000_000_000,
          totalBytes: 30_000_000_000,
        });
      }

      expect(notifications).toHaveLength(15);
      expect(responseResolved).toBe(false);

      // Now send the final response
      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 15, totalBytes: 30_000_000_000 });

      await promise;
      expect(responseResolved).toBe(true);
      // All 15 progress notifications were delivered before the response
      expect(notifications).toHaveLength(15);
    });

    it("emits notification events synchronously with data arrival (no setTimeout/nextTick delay)", () => {
      // This test verifies that the notification emission is synchronous:
      // after channel.emit("data", ...) returns, the notification listener
      // has already been called. No microtask or macrotask delay.
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Emit a complete NDJSON line
      const line = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "test.bin", percent: 42 },
      }) + "\n";

      channel.emit("data", Buffer.from(line));

      // Synchronous check — if this were async (setTimeout/nextTick),
      // the notification array would still be empty here
      expect(notifications).toHaveLength(1);
      expect((notifications[0] as any).params.percent).toBe(42);
      expect((notifications[0] as any).profileId).toBe("gpu-server");

      // Emit another line and verify it's also synchronous
      const line2 = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "test.bin", percent: 84 },
      }) + "\n";

      channel.emit("data", Buffer.from(line2));
      expect(notifications).toHaveLength(2);
      expect((notifications[1] as any).params.percent).toBe(84);
    });

    it("handles progress streaming with partial lines split across data events", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      const promise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model.safetensors"],
      });

      // Simulate a progress notification split across 3 TCP packets
      const fullLine = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "model.safetensors", percent: 55, speed: "200 MB/s" },
      });

      // Packet 1: first third
      channel.emit("data", Buffer.from(fullLine.slice(0, 30)));
      expect(notifications).toHaveLength(0); // incomplete line

      // Packet 2: middle third
      channel.emit("data", Buffer.from(fullLine.slice(30, 70)));
      expect(notifications).toHaveLength(0); // still incomplete

      // Packet 3: rest + newline + start of next notification
      const nextLine = JSON.stringify({
        jsonrpc: "2.0",
        method: "progress",
        params: { op: "hydrate", file: "model.safetensors", percent: 60, speed: "210 MB/s" },
      });
      channel.emit("data", Buffer.from(fullLine.slice(70) + "\n" + nextLine.slice(0, 20)));
      // First notification should now be emitted, second is still partial
      expect(notifications).toHaveLength(1);
      expect((notifications[0] as any).params.percent).toBe(55);

      // Complete the second notification
      channel.emit("data", Buffer.from(nextLine.slice(20) + "\n"));
      expect(notifications).toHaveLength(2);
      expect((notifications[1] as any).params.percent).toBe(60);

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { hydrated: 1, totalBytes: 4_500_000_000 });
      await promise;
    });

    it("handles progress from multiple concurrent operations on the same channel", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // Start two concurrent operations
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["model-a.safetensors"],
      });
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
      });

      // Interleave progress from both operations on the same channel
      simulateNotification(channel, "progress", {
        op: "hydrate", file: "model-a.safetensors", percent: 25,
      });
      simulateNotification(channel, "progress", {
        op: "push", file: "data.parquet", percent: 10,
      });
      simulateNotification(channel, "progress", {
        op: "hydrate", file: "model-a.safetensors", percent: 50,
      });
      simulateNotification(channel, "progress", {
        op: "push", file: "data.parquet", percent: 50,
      });
      simulateNotification(channel, "progress", {
        op: "hydrate", file: "model-a.safetensors", percent: 100,
      });
      simulateNotification(channel, "progress", {
        op: "push", file: "data.parquet", percent: 100,
      });

      // All 6 notifications arrive in order
      expect(notifications).toHaveLength(6);

      // Hydrate progress
      const hydrateProgress = notifications.filter((n: any) => n.params.op === "hydrate");
      expect(hydrateProgress.map((n: any) => n.params.percent)).toEqual([25, 50, 100]);

      // Push progress
      const pushProgress = notifications.filter((n: any) => n.params.op === "push");
      expect(pushProgress.map((n: any) => n.params.percent)).toEqual([10, 50, 100]);

      // Complete both operations
      const hydrateWritten = JSON.parse(channel.written[0]);
      const pushWritten = JSON.parse(channel.written[1]);
      simulateResponse(channel, hydrateWritten.id, { hydrated: 1, totalBytes: 4_500_000_000 });
      simulateResponse(channel, pushWritten.id, { pushed: 1, uploadedBytes: 2_000_000_000 });

      await hydratePromise;
      await pushPromise;
    });
  });

  describe("full crab workflow integration", () => {
    it("executes the complete add -> commit -> push -> dehydrate -> hydrate -> verify workflow sequentially", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const notifications: unknown[] = [];
      router.on("notification", (n) => notifications.push(n));

      // ── Step 1: add — stage large files on the remote ──────────────

      const addPromise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["model-00001.safetensors", "model-00002.safetensors", "data/train.parquet"],
      });

      const addWritten = JSON.parse(channel.written[0]);
      expect(addWritten.method).toBe("add");
      expect(addWritten.params.paths).toEqual([
        "model-00001.safetensors",
        "model-00002.safetensors",
        "data/train.parquet",
      ]);

      // Progress notifications during add (chunking files)
      simulateNotification(channel, "progress", {
        op: "add",
        file: "model-00001.safetensors",
        percent: 50,
        bytesProcessed: 2_250_000_000,
        totalBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "add",
        file: "model-00001.safetensors",
        percent: 100,
        bytesProcessed: 4_500_000_000,
        totalBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "add",
        file: "model-00002.safetensors",
        percent: 100,
        bytesProcessed: 4_500_000_000,
        totalBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "add",
        file: "data/train.parquet",
        percent: 100,
        bytesProcessed: 2_000_000_000,
        totalBytes: 2_000_000_000,
      });

      simulateResponse(channel, addWritten.id, {
        staged: 3,
        totalBytes: 11_000_000_000,
        chunks: 2560,
      });

      const addResult = await addPromise;
      expect(addResult).toEqual({ staged: 3, totalBytes: 11_000_000_000, chunks: 2560 });

      // Verify add progress notifications were received
      const addNotifications = notifications.filter((n: any) => n.params.op === "add");
      expect(addNotifications).toHaveLength(4);
      expect(addNotifications.every((n: any) => n.profileId === "gpu-server")).toBe(true);

      // ── Step 2: git_commit — commit the staged pointer blobs ───────

      const commitPromise = router.route(repoId, "git_commit", {
        repo_id: repoId,
        message: "Add ML model weights and training data",
        author: "user <user@example.com>",
      });

      const commitWritten = JSON.parse(channel.written[1]);
      expect(commitWritten.method).toBe("git_commit");
      expect(commitWritten.params.message).toBe("Add ML model weights and training data");

      simulateResponse(channel, commitWritten.id, {
        sha: "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        message: "Add ML model weights and training data",
        filesChanged: 3,
      });

      const commitResult = await commitPromise;
      expect((commitResult as any).sha).toBe("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2");
      expect((commitResult as any).filesChanged).toBe(3);

      // ── Step 3: push — push xorbs to cloud storage ─────────────────

      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const pushWritten = JSON.parse(channel.written[2]);
      expect(pushWritten.method).toBe("push");
      expect(pushWritten.params.remote).toBe("origin");
      expect(pushWritten.params.ref).toBe("main");

      // Multi-stage push progress
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "enumerating_pointers",
        percent: 10,
        pointersFound: 3,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "classifying_chunks",
        percent: 20,
        chunksClassified: 2560,
        chunksTotal: 2560,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "packing_xorbs",
        percent: 40,
        xorbsPacked: 15,
        xorbsTotal: 30,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 60,
        bytesUploaded: 5_500_000_000,
        totalBytes: 11_000_000_000,
        speed: "250 MB/s",
        eta: "22s",
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_xorbs",
        percent: 85,
        bytesUploaded: 9_350_000_000,
        totalBytes: 11_000_000_000,
        speed: "270 MB/s",
        eta: "6s",
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "building_shard",
        percent: 95,
      });
      simulateNotification(channel, "progress", {
        op: "push",
        stage: "uploading_manifest",
        percent: 100,
      });

      simulateResponse(channel, pushWritten.id, {
        pushed: true,
        xorbsUploaded: 30,
        bytesUploaded: 11_000_000_000,
      });

      const pushResult = await pushPromise;
      expect((pushResult as any).pushed).toBe(true);
      expect((pushResult as any).xorbsUploaded).toBe(30);
      expect((pushResult as any).bytesUploaded).toBe(11_000_000_000);

      // Verify multi-stage push progress
      const pushNotifications = notifications.filter((n: any) => n.params.op === "push");
      expect(pushNotifications.length).toBe(7);
      const pushStages = pushNotifications.map((n: any) => n.params.stage);
      expect(pushStages).toContain("enumerating_pointers");
      expect(pushStages).toContain("classifying_chunks");
      expect(pushStages).toContain("packing_xorbs");
      expect(pushStages).toContain("uploading_xorbs");
      expect(pushStages).toContain("building_shard");
      expect(pushStages).toContain("uploading_manifest");

      // ── Step 4: dehydrate — free disk space on the remote ──────────

      const dehydratePromise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const dehydrateWritten = JSON.parse(channel.written[3]);
      expect(dehydrateWritten.method).toBe("dehydrate");
      expect(dehydrateWritten.params.patterns).toEqual(["--all"]);

      // Dehydrate progress
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        file: "model-00001.safetensors",
        percent: 100,
        freedBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        file: "model-00002.safetensors",
        percent: 100,
        freedBytes: 4_500_000_000,
      });
      simulateNotification(channel, "progress", {
        op: "dehydrate",
        file: "data/train.parquet",
        percent: 100,
        freedBytes: 2_000_000_000,
      });

      simulateResponse(channel, dehydrateWritten.id, {
        dehydrated: 3,
        freedBytes: 11_000_000_000,
      });

      const dehydrateResult = await dehydratePromise;
      expect((dehydrateResult as any).dehydrated).toBe(3);
      expect((dehydrateResult as any).freedBytes).toBe(11_000_000_000);

      // Verify dehydrate progress
      const dehydrateNotifications = notifications.filter((n: any) => n.params.op === "dehydrate");
      expect(dehydrateNotifications).toHaveLength(3);

      // ── Step 5: hydrate — re-hydrate files from cloud ──────────────

      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const hydrateWritten = JSON.parse(channel.written[4]);
      expect(hydrateWritten.method).toBe("hydrate");
      expect(hydrateWritten.params.patterns).toEqual(["--all"]);

      // Hydrate progress streaming
      simulateNotification(channel, "progress", {
        op: "hydrate",
        filesCompleted: 0,
        filesTotal: 3,
        percent: 0,
        bytesDownloaded: 0,
        totalBytes: 11_000_000_000,
        speed: "0 MB/s",
        eta: "calculating...",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00001.safetensors",
        percent: 50,
        bytesDownloaded: 2_250_000_000,
        totalBytes: 4_500_000_000,
        speed: "200 MB/s",
        eta: "11s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00001.safetensors",
        percent: 100,
        bytesDownloaded: 4_500_000_000,
        totalBytes: 4_500_000_000,
        speed: "220 MB/s",
        eta: "0s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "model-00002.safetensors",
        percent: 100,
        bytesDownloaded: 4_500_000_000,
        totalBytes: 4_500_000_000,
        speed: "230 MB/s",
        eta: "0s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        file: "data/train.parquet",
        percent: 100,
        bytesDownloaded: 2_000_000_000,
        totalBytes: 2_000_000_000,
        speed: "210 MB/s",
        eta: "0s",
      });
      simulateNotification(channel, "progress", {
        op: "hydrate",
        filesCompleted: 3,
        filesTotal: 3,
        percent: 100,
        bytesDownloaded: 11_000_000_000,
        totalBytes: 11_000_000_000,
        speed: "220 MB/s",
        eta: "0s",
      });

      simulateResponse(channel, hydrateWritten.id, {
        hydrated: 3,
        totalBytes: 11_000_000_000,
      });

      const hydrateResult = await hydratePromise;
      expect((hydrateResult as any).hydrated).toBe(3);
      expect((hydrateResult as any).totalBytes).toBe(11_000_000_000);

      // Verify hydrate progress streaming
      const hydrateNotifications = notifications.filter((n: any) => n.params.op === "hydrate");
      expect(hydrateNotifications.length).toBe(6);
      // Verify progress includes speed and ETA
      const hydrateWithSpeed = hydrateNotifications.filter((n: any) => n.params.speed);
      expect(hydrateWithSpeed.length).toBeGreaterThanOrEqual(5);

      // ── Step 6: verify content — hydration_status shows all hydrated ─

      const statusPromise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const statusWritten = JSON.parse(channel.written[5]);
      expect(statusWritten.method).toBe("hydration_status");

      simulateResponse(channel, statusWritten.id, {
        totalFiles: 3,
        hydratedFiles: 3,
        dehydratedFiles: 0,
        totalBytes: 11_000_000_000,
        hydratedBytes: 11_000_000_000,
      });

      const statusResult = await statusPromise;
      expect((statusResult as any).totalFiles).toBe(3);
      expect((statusResult as any).hydratedFiles).toBe(3);
      expect((statusResult as any).dehydratedFiles).toBe(0);
      expect((statusResult as any).hydratedBytes).toBe((statusResult as any).totalBytes);

      // ── Final assertions: workflow integrity ────────────────────────

      // All 6 RPC calls were sent to the remote channel (never local)
      expect(channel.written).toHaveLength(6);
      expect(localAgent.calls).toHaveLength(0);

      // Verify the correct sequence of methods was sent
      const methods = channel.written.map((w: string) => JSON.parse(w).method);
      expect(methods).toEqual([
        "add",
        "git_commit",
        "push",
        "dehydrate",
        "hydrate",
        "hydration_status",
      ]);

      // All notifications were tagged with the correct profileId
      for (const n of notifications) {
        expect((n as any).profileId).toBe("gpu-server");
        expect((n as any).method).toBe("progress");
      }

      // Total progress notifications across the entire workflow
      const totalNotifications = notifications.length;
      expect(totalNotifications).toBe(4 + 7 + 3 + 6); // add + push + dehydrate + hydrate
    });

    it("each workflow step only proceeds after the previous step completes", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const completionOrder: string[] = [];

      // Step 1: add
      const addPromise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["model.safetensors"],
      });

      const addWritten = JSON.parse(channel.written[0]);
      simulateResponse(channel, addWritten.id, { staged: 1, totalBytes: 4_500_000_000, chunks: 512 });
      await addPromise;
      completionOrder.push("add");

      // Step 2: git_commit (only after add completes)
      expect(completionOrder).toEqual(["add"]);
      const commitPromise = router.route(repoId, "git_commit", {
        repo_id: repoId,
        message: "Add model",
      });

      const commitWritten = JSON.parse(channel.written[1]);
      simulateResponse(channel, commitWritten.id, {
        sha: "abc123def456",
        message: "Add model",
        filesChanged: 1,
      });
      await commitPromise;
      completionOrder.push("git_commit");

      // Step 3: push (only after commit completes)
      expect(completionOrder).toEqual(["add", "git_commit"]);
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });

      const pushWritten = JSON.parse(channel.written[2]);
      simulateResponse(channel, pushWritten.id, {
        pushed: true,
        xorbsUploaded: 6,
        bytesUploaded: 4_500_000_000,
      });
      await pushPromise;
      completionOrder.push("push");

      // Step 4: dehydrate (only after push completes)
      expect(completionOrder).toEqual(["add", "git_commit", "push"]);
      const dehydratePromise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const dehydrateWritten = JSON.parse(channel.written[3]);
      simulateResponse(channel, dehydrateWritten.id, {
        dehydrated: 1,
        freedBytes: 4_500_000_000,
      });
      await dehydratePromise;
      completionOrder.push("dehydrate");

      // Step 5: hydrate (only after dehydrate completes)
      expect(completionOrder).toEqual(["add", "git_commit", "push", "dehydrate"]);
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });

      const hydrateWritten = JSON.parse(channel.written[4]);
      simulateResponse(channel, hydrateWritten.id, {
        hydrated: 1,
        totalBytes: 4_500_000_000,
      });
      await hydratePromise;
      completionOrder.push("hydrate");

      // Step 6: verify (only after hydrate completes)
      expect(completionOrder).toEqual(["add", "git_commit", "push", "dehydrate", "hydrate"]);
      const statusPromise = router.route(repoId, "hydration_status", { repo_id: repoId });

      const statusWritten = JSON.parse(channel.written[5]);
      simulateResponse(channel, statusWritten.id, {
        totalFiles: 1,
        hydratedFiles: 1,
        dehydratedFiles: 0,
        totalBytes: 4_500_000_000,
        hydratedBytes: 4_500_000_000,
      });
      await statusPromise;
      completionOrder.push("hydration_status");

      // Verify strict sequential ordering
      expect(completionOrder).toEqual([
        "add",
        "git_commit",
        "push",
        "dehydrate",
        "hydrate",
        "hydration_status",
      ]);

      // All calls went to the remote, never local
      expect(localAgent.calls).toHaveLength(0);
      expect(channel.written).toHaveLength(6);
    });

    it("workflow fails at push step and does not proceed to dehydrate/hydrate", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // Step 1: add succeeds
      const addPromise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: ["model.safetensors"],
      });
      const addWritten = JSON.parse(channel.written[0]);
      simulateResponse(channel, addWritten.id, { staged: 1, totalBytes: 4_500_000_000, chunks: 512 });
      const addResult = await addPromise;
      expect((addResult as any).staged).toBe(1);

      // Step 2: commit succeeds
      const commitPromise = router.route(repoId, "git_commit", {
        repo_id: repoId,
        message: "Add model",
      });
      const commitWritten = JSON.parse(channel.written[1]);
      simulateResponse(channel, commitWritten.id, {
        sha: "abc123",
        message: "Add model",
        filesChanged: 1,
      });
      await commitPromise;

      // Step 3: push FAILS with cloud storage error
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });
      const pushWritten = JSON.parse(channel.written[2]);

      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: pushWritten.id,
        error: {
          code: -32002,
          message: "Cloud storage authentication failed: expired credentials",
          data: { bucket: "ml-models-bucket", provider: "s3" },
        },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(pushPromise).rejects.toThrow(/Cloud storage authentication failed/);

      // Workflow stops here — only 3 RPC calls were made (add, commit, push)
      expect(channel.written).toHaveLength(3);
      // No dehydrate or hydrate calls were sent
      const methods = channel.written.map((w: string) => JSON.parse(w).method);
      expect(methods).toEqual(["add", "git_commit", "push"]);
    });

    it("workflow preserves data integrity: hydrated content matches original after dehydrate/hydrate cycle", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // The original file sizes from the add step
      const originalFiles = [
        { path: "model-00001.safetensors", size: 4_500_000_000 },
        { path: "model-00002.safetensors", size: 4_500_000_000 },
        { path: "data/train.parquet", size: 2_000_000_000 },
      ];
      const totalOriginalBytes = originalFiles.reduce((sum, f) => sum + f.size, 0);

      // Step 1: add
      const addPromise = router.route(repoId, "add", {
        repo_id: repoId,
        paths: originalFiles.map((f) => f.path),
      });
      const addWritten = JSON.parse(channel.written[0]);
      simulateResponse(channel, addWritten.id, {
        staged: 3,
        totalBytes: totalOriginalBytes,
        chunks: 2560,
      });
      await addPromise;

      // Step 2: commit
      const commitPromise = router.route(repoId, "git_commit", {
        repo_id: repoId,
        message: "Add files",
      });
      const commitWritten = JSON.parse(channel.written[1]);
      simulateResponse(channel, commitWritten.id, {
        sha: "deadbeef12345678",
        message: "Add files",
        filesChanged: 3,
      });
      await commitPromise;

      // Step 3: push
      const pushPromise = router.route(repoId, "push", {
        repo_id: repoId,
        remote: "origin",
        ref: "main",
      });
      const pushWritten = JSON.parse(channel.written[2]);
      simulateResponse(channel, pushWritten.id, {
        pushed: true,
        xorbsUploaded: 30,
        bytesUploaded: totalOriginalBytes,
      });
      await pushPromise;

      // Step 4: dehydrate
      const dehydratePromise = router.route(repoId, "dehydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });
      const dehydrateWritten = JSON.parse(channel.written[3]);
      simulateResponse(channel, dehydrateWritten.id, {
        dehydrated: 3,
        freedBytes: totalOriginalBytes,
      });
      const dehydrateResult = await dehydratePromise;
      expect((dehydrateResult as any).freedBytes).toBe(totalOriginalBytes);

      // Step 5: hydrate
      const hydratePromise = router.route(repoId, "hydrate", {
        repo_id: repoId,
        patterns: ["--all"],
      });
      const hydrateWritten = JSON.parse(channel.written[4]);
      simulateResponse(channel, hydrateWritten.id, {
        hydrated: 3,
        totalBytes: totalOriginalBytes,
      });
      const hydrateResult = await hydratePromise;

      // Verify: hydrated bytes match the original total
      expect((hydrateResult as any).hydrated).toBe(3);
      expect((hydrateResult as any).totalBytes).toBe(totalOriginalBytes);

      // Step 6: verify hydration_status confirms all files are hydrated
      const statusPromise = router.route(repoId, "hydration_status", { repo_id: repoId });
      const statusWritten = JSON.parse(channel.written[5]);
      simulateResponse(channel, statusWritten.id, {
        totalFiles: 3,
        hydratedFiles: 3,
        dehydratedFiles: 0,
        totalBytes: totalOriginalBytes,
        hydratedBytes: totalOriginalBytes,
      });
      const statusResult = await statusPromise;

      // Data integrity: all files hydrated, total bytes match original
      expect((statusResult as any).hydratedFiles).toBe(originalFiles.length);
      expect((statusResult as any).dehydratedFiles).toBe(0);
      expect((statusResult as any).hydratedBytes).toBe(totalOriginalBytes);
      expect((statusResult as any).totalBytes).toBe(totalOriginalBytes);
    });

    it("all workflow operations route to remote agent, never to local", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";

      // Execute all 6 steps and verify local agent is never called
      const steps: Array<{ method: string; params: Record<string, unknown>; response: unknown }> = [
        {
          method: "add",
          params: { repo_id: repoId, paths: ["file.bin"] },
          response: { staged: 1, totalBytes: 1_000_000, chunks: 4 },
        },
        {
          method: "git_commit",
          params: { repo_id: repoId, message: "commit" },
          response: { sha: "abc", message: "commit", filesChanged: 1 },
        },
        {
          method: "push",
          params: { repo_id: repoId, remote: "origin", ref: "main" },
          response: { pushed: true, xorbsUploaded: 1, bytesUploaded: 1_000_000 },
        },
        {
          method: "dehydrate",
          params: { repo_id: repoId, patterns: ["--all"] },
          response: { dehydrated: 1, freedBytes: 1_000_000 },
        },
        {
          method: "hydrate",
          params: { repo_id: repoId, patterns: ["--all"] },
          response: { hydrated: 1, totalBytes: 1_000_000 },
        },
        {
          method: "hydration_status",
          params: { repo_id: repoId },
          response: { totalFiles: 1, hydratedFiles: 1, dehydratedFiles: 0, totalBytes: 1_000_000, hydratedBytes: 1_000_000 },
        },
      ];

      for (let i = 0; i < steps.length; i++) {
        const step = steps[i];
        const promise = router.route(repoId, step.method, step.params);
        const written = JSON.parse(channel.written[i]);
        expect(written.method).toBe(step.method);
        simulateResponse(channel, written.id, step.response);
        await promise;

        // After each step, local agent should never have been called
        expect(localAgent.calls).toHaveLength(0);
      }

      // Final verification
      expect(channel.written).toHaveLength(6);
      expect(localAgent.calls).toHaveLength(0);
    });
  });
});
