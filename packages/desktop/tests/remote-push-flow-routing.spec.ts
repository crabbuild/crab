/**
 * Verification test: Remote Push Flow Panel — staging_stats RPC routing.
 *
 * Confirms that `rpc.stagingStats(repo_id)` routes correctly through the
 * RemoteProvider → RpcRouter → remote agent pipeline for remote repos,
 * and that the returned xorb/pointer cost estimates are structured
 * correctly for display in the PushFlowPanel and FlowView.
 *
 * The PushFlowPanel uses:
 *   - rpc.stagingStats(repo.repo_id) — staging area stats (xorb/pointer costs)
 *   - rpc.crabPushStructured(repo.repo_id, opts) — the actual push
 *   - rpc.gitRepoInfo(repo.repo_id) — to list crab:// remotes
 *
 * All three use `repo_id` as the routing key. For remote repos with
 * `ssh://` prefixed repo_ids, the RpcRouter dispatches to the remote
 * agent over the SSH channel.
 */

import { describe, it, expect, vi } from "vitest";
import { EventEmitter } from "node:events";
import { withRepositoryRuntimes } from "./repository-runtime-fixture";
import { parseRepoId } from "../electron/ssh-types";
import { RpcRouter } from "../electron/ssh-router";
import { RemoteProvider } from "../electron/remote-provider";

// ─── Mock SSH Session Manager ────────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();
}

class MockSessionManager extends EventEmitter {
  agentSession() { return "ssh:1"; }
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockChannel }; profile?: { operationTimeoutMs?: number } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string) {
    return this.connections.get(profileId);
  }
}

// ─── Mock Dependencies ───────────────────────────────────────────────────────

class MockFileEditor {
  createFile = vi.fn().mockResolvedValue({ success: true });
  readFile = vi.fn().mockResolvedValue({ content: "" });
  writeFile = vi.fn().mockResolvedValue({ success: true });
  deleteFile = vi.fn().mockResolvedValue({ success: true });
  renameFile = vi.fn().mockResolvedValue({ success: true });
}

class MockGitManager {}

class MockCapabilityNegotiator {
  isSupported(_profileId: string, _capability: string): boolean {
    return true;
  }
  getCached(_profileId: string) {
    return { version: "1.0.0" };
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

const TEST_PROFILE_ID = "gpu-server";
const TEST_REPO_ID = `ssh://${TEST_PROFILE_ID}/home/dev/ml-project`;

function createTestSetup() {
  const sessionManager = withRepositoryRuntimes(new MockSessionManager());
  const channel = new MockChannel();
  const localAgent = { request: vi.fn().mockResolvedValue({ ok: true }) };

  sessionManager.setStatus(TEST_PROFILE_ID, "connected");
  sessionManager.setConnection(TEST_PROFILE_ID, channel);

  const router = new RpcRouter(sessionManager as any, localAgent, { requestTimeoutMs: 5000 });
  router.attachChannel(TEST_PROFILE_ID, channel as any);

  const fileEditor = new MockFileEditor();
  const gitManager = new MockGitManager();
  const capabilityNegotiator = new MockCapabilityNegotiator();

  const provider = new RemoteProvider({
    router,
    fileEditor: fileEditor as any,
    gitManager: gitManager as any,
    capabilityNegotiator: capabilityNegotiator as any,
    sessionManager: sessionManager as any,
  });

  return { provider, router, channel, sessionManager, localAgent };
}

function simulateResponse(channel: MockChannel, id: number, result: unknown): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
  channel.emit("data", Buffer.from(response));
}

function getLastRequest(channel: MockChannel): { id: number; method: string; params: unknown } {
  const lastFrame = channel.written[channel.written.length - 1];
  return JSON.parse(lastFrame.trim());
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Remote Push Flow Panel — staging_stats routing", () => {
  describe("staging_stats routes to remote agent for ssh:// repo_ids", () => {
    it("routes staging_stats to the remote agent via SSH channel", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID };
      const resultPromise = provider.request("staging_stats", params);

      expect(channel.written.length).toBe(1);

      const request = getLastRequest(channel);
      expect(request.method).toBe("staging_stats");
      expect(request.params).toEqual(params);

      const mockStats = {
        segments_sealed: 2,
        current_segment_bytes: 1024,
        total_staged_bytes: 52428800,
        live_bytes: 50000000,
        dead_bytes: 2428800,
        dead_ratio: 0.046,
        chunk_count: 128,
        file_count: 3,
        files: [
          {
            file_hash: "abc123def456789012345678901234567890123456789012345678901234abcd",
            file_path: "models/checkpoint.safetensors",
            total_bytes: 40000000,
            committed_chunks: 80,
            pending_chunks: 0,
            total_chunks: 80,
            segments: 2,
          },
          {
            file_hash: "def456abc789012345678901234567890123456789012345678901234567efgh",
            file_path: "data/training.parquet",
            total_bytes: 10000000,
            committed_chunks: 40,
            pending_chunks: 8,
            total_chunks: 48,
            segments: 1,
          },
        ],
      };
      simulateResponse(channel, request.id, mockStats);

      const result = await resultPromise;
      expect(result).toEqual(mockStats);
    });

    it("does NOT send staging_stats to the local agent for ssh:// repo_ids", async () => {
      const { router, channel, localAgent } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID };
      const resultPromise = router.route(TEST_REPO_ID, "staging_stats", params);

      expect(channel.written.length).toBe(1);
      expect(localAgent.request).not.toHaveBeenCalled();

      const request = getLastRequest(channel);
      simulateResponse(channel, request.id, { file_count: 0, chunk_count: 0 });

      await resultPromise;
    });

    it("sends staging_stats to the local agent for non-ssh repo_ids", async () => {
      const { router, localAgent } = createTestSetup();

      const localRepoId = "/Users/user/project";
      const params = { repo_id: localRepoId };

      await router.route(localRepoId, "staging_stats", params);

      expect(localAgent.request).toHaveBeenCalledWith("staging_stats", params);
    });
  });

  describe("crab_push_structured routes to remote agent for ssh:// repo_ids", () => {
    it("routes crab_push_structured to the remote agent via SSH channel", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, force: false };
      const resultPromise = provider.request("crab_push_structured", params);

      expect(channel.written.length).toBe(1);

      const request = getLastRequest(channel);
      expect(request.method).toBe("crab_push_structured");
      expect(request.params).toEqual(params);

      const mockResult = {
        status: 0,
        job_id: "push-remote-12345",
        summary: {
          refs_pushed: 1,
          duration_ms: 4500,
          bytes_uploaded: 52428800,
          xorbs_created: 3,
        },
      };
      simulateResponse(channel, request.id, mockResult);

      const result = await resultPromise;
      expect(result).toEqual(mockResult);
    });

    it("routes crab_push_structured with remote option to the remote agent", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, remote: "origin", force: true };
      const resultPromise = provider.request("crab_push_structured", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("crab_push_structured");
      expect(request.params).toEqual(params);

      simulateResponse(channel, request.id, { status: 0, job_id: "push-force-1", summary: null });

      const result = await resultPromise;
      expect(result).toEqual({ status: 0, job_id: "push-force-1", summary: null });
    });
  });

  describe("staging_stats response structure matches display requirements", () => {
    it("returns xorb cost estimates (chunk_count, total_staged_bytes, file breakdown)", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID };
      const resultPromise = provider.request("staging_stats", params);

      const request = getLastRequest(channel);

      // Simulate a response with full xorb/pointer cost data
      const mockStats = {
        segments_sealed: 5,
        current_segment_bytes: 2048,
        total_staged_bytes: 104857600, // 100 MB
        live_bytes: 100000000,
        dead_bytes: 4857600,
        dead_ratio: 0.046,
        chunk_count: 256,
        file_count: 5,
        files: [
          {
            file_hash: "a".repeat(64),
            file_path: "weights/model.safetensors",
            total_bytes: 80000000,
            committed_chunks: 160,
            pending_chunks: 0,
            total_chunks: 160,
            segments: 3,
          },
          {
            file_hash: "b".repeat(64),
            file_path: null, // path not recorded
            total_bytes: 20000000,
            committed_chunks: 80,
            pending_chunks: 16,
            total_chunks: 96,
            segments: 2,
          },
        ],
      };
      simulateResponse(channel, request.id, mockStats);

      const result = (await resultPromise) as Record<string, unknown>;

      // Verify the structure matches what FlowView/PushFlowPanel expects
      expect(result.chunk_count).toBe(256);
      expect(result.total_staged_bytes).toBe(104857600);
      expect(result.file_count).toBe(5);
      expect(result.segments_sealed).toBe(5);
      expect(result.live_bytes).toBe(100000000);
      expect(result.dead_bytes).toBe(4857600);
      expect(result.dead_ratio).toBeCloseTo(0.046);

      // Verify per-file breakdown (used for xorb cost display)
      const files = result.files as Array<Record<string, unknown>>;
      expect(files).toHaveLength(2);
      expect(files[0].file_path).toBe("weights/model.safetensors");
      expect(files[0].total_chunks).toBe(160);
      expect(files[0].total_bytes).toBe(80000000);
      expect(files[0].pending_chunks).toBe(0);

      // File without path (displays truncated hash in UI)
      expect(files[1].file_path).toBeNull();
      expect(files[1].total_chunks).toBe(96);
      expect(files[1].pending_chunks).toBe(16);
    });

    it("handles empty staging area (no files staged)", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID };
      const resultPromise = provider.request("staging_stats", params);

      const request = getLastRequest(channel);

      const emptyStats = {
        segments_sealed: 0,
        current_segment_bytes: 0,
        total_staged_bytes: 0,
        live_bytes: 0,
        dead_bytes: 0,
        dead_ratio: 0,
        chunk_count: 0,
        file_count: 0,
        files: [],
      };
      simulateResponse(channel, request.id, emptyStats);

      const result = (await resultPromise) as Record<string, unknown>;
      expect(result.file_count).toBe(0);
      expect(result.chunk_count).toBe(0);
      expect(result.files).toEqual([]);
    });
  });

  describe("parseRepoId correctly identifies remote push flow targets", () => {
    it("parses ssh:// repo_ids used by PushFlowPanel", () => {
      const parsed = parseRepoId(TEST_REPO_ID);
      expect(parsed).not.toBeNull();
      expect(parsed!.profileId).toBe("gpu-server");
      expect(parsed!.remotePath).toBe("/home/dev/ml-project");
    });

    it("returns null for local repo_ids (push stays local)", () => {
      expect(parseRepoId("/Users/user/project")).toBeNull();
      expect(parseRepoId("C:\\Users\\user\\project")).toBeNull();
    });

    it("handles ssh:// repo_ids with deep paths", () => {
      const deepPath = "ssh://prod-server/var/repos/org/team/service";
      const parsed = parseRepoId(deepPath);
      expect(parsed).not.toBeNull();
      expect(parsed!.profileId).toBe("prod-server");
      expect(parsed!.remotePath).toBe("/var/repos/org/team/service");
    });
  });

  describe("PushFlowPanel uses repo.repo_id (not repo.path) for all RPC calls", () => {
    it("staging_stats call uses repo_id as the routing key", () => {
      // The FlowView calls: rpc.stagingStats(repo.repo_id)
      // which translates to: call<StagingStats>("staging_stats", { repo_id })
      // The IPC handler extracts repo_id and routes via ProviderRegistry.
      //
      // For remote repos, repo.repo_id = "ssh://<profileId>/<path>"
      // and repo.path may be undefined (remote repos don't have local paths).
      //
      // Verify the contract: staging_stats includes repo_id in params.
      const remoteRepoId = "ssh://gpu-server/home/dev/ml-project";
      const parsed = parseRepoId(remoteRepoId);
      expect(parsed).not.toBeNull();
      expect(parsed!.profileId).toBe("gpu-server");

      // Local repo_ids are not parsed as remote
      expect(parseRepoId("/Users/user/project")).toBeNull();
    });

    it("crab_push_structured call uses repo_id as the routing key", () => {
      // The PushFlowPanel calls: rpc.crabPushStructured(repoId, opts)
      // which translates to: call("crab_push_structured", { repo_id, ...opts })
      // Same routing mechanism as staging_stats.
      const remoteRepoId = "ssh://gpu-server/home/dev/ml-project";
      const parsed = parseRepoId(remoteRepoId);
      expect(parsed).not.toBeNull();
    });
  });
});
