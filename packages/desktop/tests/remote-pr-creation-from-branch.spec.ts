/**
 * Verification test: PR creation from a remote branch.
 *
 * Confirms the end-to-end flow for creating a pull request from a
 * remote repo:
 *   1. Query the remote agent for the current branch name via `list_refs`
 *   2. Call `forge_create_pr` with the branch name and remote URL
 *
 * This verifies that:
 *   - `list_refs` routes to the remote agent and returns branch info
 *   - The head branch from `list_refs` is correctly passed to `forge_create_pr`
 *   - `forge_create_pr` routes to the remote agent (not local)
 *   - No `repo.path` guards block the flow for remote repos
 *
 * Validates: Requirements 10.2
 */

import { describe, it, expect, vi } from "vitest";
import { EventEmitter } from "node:events";
import { RpcRouter, parseRepoId } from "../electron/ssh-router";
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
const TEST_REPO_ID = `ssh://${TEST_PROFILE_ID}/home/dev/my-project`;

function createTestSetup() {
  const sessionManager = new MockSessionManager();
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

function getRequest(channel: MockChannel, index: number): { id: number; method: string; params: unknown } {
  return JSON.parse(channel.written[index].trim());
}

function getLastRequest(channel: MockChannel): { id: number; method: string; params: unknown } {
  return getRequest(channel, channel.written.length - 1);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("PR creation from remote branch", () => {
  describe("Step 1: Query remote agent for current branch name", () => {
    it("routes list_refs to the remote agent via SSH channel", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, kinds: ["branch"] };
      const resultPromise = provider.request("list_refs", params);

      expect(channel.written.length).toBe(1);
      const request = getLastRequest(channel);
      expect(request.method).toBe("list_refs");
      expect(request.params).toEqual(params);

      // Simulate remote agent returning branch list with HEAD indicator
      const mockRefs = {
        refs: [
          { name: "main", kind: "branch", is_head: false, sha: "aaa111" },
          { name: "feature/remote-pr", kind: "branch", is_head: true, sha: "bbb222" },
          { name: "fix/bug-123", kind: "branch", is_head: false, sha: "ccc333" },
        ],
      };
      simulateResponse(channel, request.id, mockRefs);

      const result = await resultPromise;
      expect(result).toEqual(mockRefs);

      // Verify we can extract the current branch (is_head: true)
      const refs = (result as { refs: Array<{ name: string; is_head: boolean }> }).refs;
      const headBranch = refs.find((r) => r.is_head);
      expect(headBranch).toBeDefined();
      expect(headBranch!.name).toBe("feature/remote-pr");
    });

    it("does NOT route list_refs to the local agent for ssh:// repo_ids", async () => {
      const { router, channel, localAgent } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, kinds: ["branch"] };
      const resultPromise = router.route(TEST_REPO_ID, "list_refs", params);

      expect(channel.written.length).toBe(1);
      expect(localAgent.request).not.toHaveBeenCalled();

      const request = getLastRequest(channel);
      simulateResponse(channel, request.id, { refs: [] });
      await resultPromise;
    });
  });

  describe("Step 2: Create PR with branch name from remote agent", () => {
    it("routes forge_create_pr to the remote agent with the correct head branch", async () => {
      const { provider, channel } = createTestSetup();

      // Simulate the full flow: first get the branch, then create the PR
      const branchName = "feature/remote-pr";

      const createParams = {
        repo_id: TEST_REPO_ID,
        title: "Add remote workspace support",
        body: "This PR adds full parity for remote workspaces.",
        head: branchName,
        base: "main",
        draft: false,
      };
      const resultPromise = provider.request("forge_create_pr", createParams);

      expect(channel.written.length).toBe(1);
      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_create_pr");
      expect(request.params).toEqual(createParams);

      // Verify the head branch is correctly passed through
      const sentParams = request.params as Record<string, unknown>;
      expect(sentParams.head).toBe("feature/remote-pr");
      expect(sentParams.base).toBe("main");
      expect(sentParams.repo_id).toBe(TEST_REPO_ID);

      // Simulate successful PR creation response
      simulateResponse(channel, request.id, {
        number: 42,
        url: "https://github.com/org/my-project/pull/42",
      });

      const result = await resultPromise;
      expect(result).toEqual({
        number: 42,
        url: "https://github.com/org/my-project/pull/42",
      });
    });
  });

  describe("End-to-end: list_refs → forge_create_pr on remote repo", () => {
    it("queries branch then creates PR, both routed to remote agent", async () => {
      const { provider, channel } = createTestSetup();

      // Step 1: Query current branch
      const listRefsPromise = provider.request("list_refs", {
        repo_id: TEST_REPO_ID,
        kinds: ["branch"],
      });

      const listRefsReq = getLastRequest(channel);
      expect(listRefsReq.method).toBe("list_refs");

      simulateResponse(channel, listRefsReq.id, {
        refs: [
          { name: "main", kind: "branch", is_head: false, sha: "aaa" },
          { name: "feat/new-feature", kind: "branch", is_head: true, sha: "bbb" },
        ],
      });

      const refsResult = await listRefsPromise;
      const refs = (refsResult as { refs: Array<{ name: string; is_head: boolean }> }).refs;
      const currentBranch = refs.find((r) => r.is_head)!.name;
      expect(currentBranch).toBe("feat/new-feature");

      // Step 2: Create PR using the branch name from step 1
      const createPrPromise = provider.request("forge_create_pr", {
        repo_id: TEST_REPO_ID,
        title: "New feature implementation",
        body: "",
        head: currentBranch,
        base: "main",
        draft: false,
      });

      const createPrReq = getLastRequest(channel);
      expect(createPrReq.method).toBe("forge_create_pr");
      expect((createPrReq.params as Record<string, unknown>).head).toBe("feat/new-feature");

      simulateResponse(channel, createPrReq.id, {
        number: 7,
        url: "https://github.com/org/repo/pull/7",
      });

      const prResult = await createPrPromise;
      expect(prResult).toEqual({ number: 7, url: "https://github.com/org/repo/pull/7" });

      // Both calls went to the remote agent (SSH channel), not local
      expect(channel.written.length).toBe(2);
    });
  });

  describe("Remote repo_id parsing for PR flow", () => {
    it("correctly identifies ssh:// repo_ids for routing", () => {
      const parsed = parseRepoId(TEST_REPO_ID);
      expect(parsed).not.toBeNull();
      expect(parsed!.profileId).toBe("gpu-server");
      expect(parsed!.remotePath).toBe("/home/dev/my-project");
    });

    it("does not route local repo_ids through SSH", () => {
      expect(parseRepoId("/Users/dev/project")).toBeNull();
      expect(parseRepoId("C:\\Users\\dev\\project")).toBeNull();
    });
  });

  describe("PR creation with draft flag from remote branch", () => {
    it("correctly passes draft: true to the remote agent", async () => {
      const { provider, channel } = createTestSetup();

      const params = {
        repo_id: TEST_REPO_ID,
        title: "WIP: Remote workspace",
        body: "Work in progress",
        head: "wip/remote-workspace",
        base: "develop",
        draft: true,
      };
      const resultPromise = provider.request("forge_create_pr", params);

      const request = getLastRequest(channel);
      expect((request.params as Record<string, unknown>).draft).toBe(true);
      expect((request.params as Record<string, unknown>).base).toBe("develop");

      simulateResponse(channel, request.id, {
        number: 100,
        url: "https://github.com/org/repo/pull/100",
      });

      const result = await resultPromise;
      expect((result as { number: number }).number).toBe(100);
    });
  });

  describe("Error handling during remote PR creation", () => {
    it("propagates forge API errors from the remote agent", async () => {
      const { provider, channel } = createTestSetup();

      const params = {
        repo_id: TEST_REPO_ID,
        title: "New PR",
        body: "",
        head: "feature-branch",
        base: "main",
        draft: false,
      };
      const resultPromise = provider.request("forge_create_pr", params);

      const request = getLastRequest(channel);

      // Simulate an error response from the remote agent
      const errorResponse = JSON.stringify({
        jsonrpc: "2.0",
        id: request.id,
        error: { code: -32000, message: "Forge auth token expired" },
      }) + "\n";
      channel.emit("data", Buffer.from(errorResponse));

      await expect(resultPromise).rejects.toThrow();
    });

    it("rejects forge_create_pr during SSH disconnection", async () => {
      const { provider, sessionManager } = createTestSetup();

      sessionManager.setStatus(TEST_PROFILE_ID, "disconnected");

      const params = {
        repo_id: TEST_REPO_ID,
        title: "New PR",
        body: "",
        head: "feature-branch",
        base: "main",
        draft: false,
      };

      await expect(provider.request("forge_create_pr", params)).rejects.toThrow();
    });
  });
});
