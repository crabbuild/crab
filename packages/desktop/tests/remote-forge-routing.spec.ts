/**
 * Verification test: Remote forge/PR RPC routing.
 *
 * Confirms that all forge RPC methods route correctly through the
 * RemoteProvider → RpcRouter → remote agent pipeline for remote repos.
 *
 * The remote agent's `forge/handlers.rs` handles:
 *   - forge_list_prs
 *   - forge_create_pr
 *   - forge_pr_detail
 *   - forge_pr_comment
 *   - forge_submit_review
 *   - forge_list_checks
 *
 * The forge API calls are network-based (they call GitHub/GitLab/etc.
 * over HTTPS) regardless of repo location. They work for remote repos
 * as long as:
 *   1. The SSH connection to the remote agent is active
 *   2. The `forge_provider` is set on the RepoHandle
 *   3. The remote agent has forge credentials configured
 *
 * The RpcRouter.route() is method-agnostic — it forwards any method
 * string to the remote agent over the SSH channel. This test verifies
 * that the RemoteProvider.request() correctly delegates each forge
 * verb through the router without filtering or special-casing.
 */

import { describe, it, expect, vi } from "vitest";
import { EventEmitter } from "node:events";
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

const TEST_PROFILE_ID = "my-server";
const TEST_REPO_ID = `ssh://${TEST_PROFILE_ID}/home/user/project`;

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

/**
 * Simulate a JSON-RPC response arriving on the channel for a given
 * request ID.
 */
function simulateResponse(channel: MockChannel, id: number, result: unknown): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
  channel.emit("data", Buffer.from(response));
}

/**
 * Extract the JSON-RPC request from the channel's written data.
 */
function getLastRequest(channel: MockChannel): { id: number; method: string; params: unknown } {
  const lastFrame = channel.written[channel.written.length - 1];
  return JSON.parse(lastFrame.trim());
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Remote forge/PR RPC routing", () => {
  const FORGE_METHODS = [
    "forge_list_prs",
    "forge_create_pr",
    "forge_pr_detail",
    "forge_pr_comment",
    "forge_submit_review",
    "forge_list_checks",
  ] as const;

  describe("RemoteProvider routes all forge verbs through RpcRouter", () => {
    for (const method of FORGE_METHODS) {
      it(`routes "${method}" to the remote agent via SSH channel`, async () => {
        const { provider, channel } = createTestSetup();

        const params = { repo_id: TEST_REPO_ID, pr_number: 42 };

        // Fire the request (it will pend until we simulate a response)
        const resultPromise = provider.request(method, params);

        // The router should have written a JSON-RPC frame to the channel
        expect(channel.written.length).toBe(1);

        const request = getLastRequest(channel);
        expect(request.method).toBe(method);
        expect(request.params).toEqual(params);

        // Simulate the remote agent responding
        const mockResult = { prs: [], ok: true };
        simulateResponse(channel, request.id, mockResult);

        const result = await resultPromise;
        expect(result).toEqual(mockResult);
      });
    }
  });

  describe("RpcRouter routes forge methods to remote (not local) agent", () => {
    it("does NOT send forge calls to the local agent for ssh:// repo_ids", async () => {
      const { router, channel, localAgent } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID };
      const resultPromise = router.route(TEST_REPO_ID, "forge_list_prs", params);

      // Should have written to the SSH channel, not the local agent
      expect(channel.written.length).toBe(1);
      expect(localAgent.request).not.toHaveBeenCalled();

      const request = getLastRequest(channel);
      simulateResponse(channel, request.id, { prs: [] });

      await resultPromise;
    });

    it("sends forge calls to the local agent for non-ssh repo_ids", async () => {
      const { router, localAgent } = createTestSetup();

      const localRepoId = "/Users/user/project";
      const params = { repo_id: localRepoId };

      await router.route(localRepoId, "forge_list_prs", params);

      expect(localAgent.request).toHaveBeenCalledWith("forge_list_prs", params);
    });
  });

  describe("forge_list_prs returns PR list from remote agent", () => {
    it("returns the full PR list as received from the remote agent", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID };
      const resultPromise = provider.request("forge_list_prs", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_list_prs");

      const mockPrs = {
        prs: [
          {
            number: 1,
            title: "Add feature X",
            author: "alice",
            status: "open",
            source_branch: "feature-x",
            target_branch: "main",
            updated_at: "2024-01-15T10:00:00Z",
            assignees: [],
            is_mine: true,
            is_assigned_to_me: false,
          },
          {
            number: 2,
            title: "Fix bug Y",
            author: "bob",
            status: "draft",
            source_branch: "fix-y",
            target_branch: "main",
            updated_at: "2024-01-14T09:00:00Z",
            assignees: [],
            is_mine: false,
            is_assigned_to_me: true,
          },
        ],
      };
      simulateResponse(channel, request.id, mockPrs);

      const result = await resultPromise;
      expect(result).toEqual(mockPrs);
    });
  });

  describe("forge_create_pr passes all parameters to remote agent", () => {
    it("forwards title, body, head, base, and draft fields", async () => {
      const { provider, channel } = createTestSetup();

      const params = {
        repo_id: TEST_REPO_ID,
        title: "New feature",
        body: "Description of the PR",
        head: "feature-branch",
        base: "main",
        draft: false,
      };
      const resultPromise = provider.request("forge_create_pr", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_create_pr");
      expect(request.params).toEqual(params);

      simulateResponse(channel, request.id, { number: 99, url: "https://github.com/org/repo/pull/99" });

      const result = await resultPromise;
      expect(result).toEqual({ number: 99, url: "https://github.com/org/repo/pull/99" });
    });
  });

  describe("forge_pr_detail returns full PR detail from remote agent", () => {
    it("returns detail including timeline, changed_files, and checks", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, pr_number: 42 };
      const resultPromise = provider.request("forge_pr_detail", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_pr_detail");

      const mockDetail = {
        number: 42,
        title: "Big refactor",
        author: "alice",
        status: "open",
        body: "This PR refactors the core module.",
        timeline: [{ type: "comment", author: "bob", body: "LGTM" }],
        changed_files: [{ path: "src/core.ts", additions: 50, deletions: 20 }],
        checks: [{ name: "CI", status: "success" }],
      };
      simulateResponse(channel, request.id, mockDetail);

      const result = await resultPromise;
      expect(result).toEqual(mockDetail);
    });
  });

  describe("forge_pr_comment sends comment to remote agent", () => {
    it("forwards pr_number and body to the remote agent", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, pr_number: 42, body: "Great work!" };
      const resultPromise = provider.request("forge_pr_comment", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_pr_comment");
      expect(request.params).toEqual(params);

      simulateResponse(channel, request.id, { ok: true, pr_number: 42 });

      const result = await resultPromise;
      expect(result).toEqual({ ok: true, pr_number: 42 });
    });
  });

  describe("forge_submit_review sends review to remote agent", () => {
    it("forwards review event (approve/request_changes/comment) to remote", async () => {
      const { provider, channel } = createTestSetup();

      const params = {
        repo_id: TEST_REPO_ID,
        pr_number: 42,
        event: "approve",
        body: "Looks good to me!",
      };
      const resultPromise = provider.request("forge_submit_review", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_submit_review");
      expect(request.params).toEqual(params);

      simulateResponse(channel, request.id, { ok: true, pr_number: 42 });

      const result = await resultPromise;
      expect(result).toEqual({ ok: true, pr_number: 42 });
    });
  });

  describe("forge_list_checks returns CI checks from remote agent", () => {
    it("forwards sha and returns check results", async () => {
      const { provider, channel } = createTestSetup();

      const params = { repo_id: TEST_REPO_ID, sha: "abc123def456" };
      const resultPromise = provider.request("forge_list_checks", params);

      const request = getLastRequest(channel);
      expect(request.method).toBe("forge_list_checks");
      expect(request.params).toEqual(params);

      const mockChecks = {
        checks: [
          { name: "build", status: "success", conclusion: "success" },
          { name: "lint", status: "completed", conclusion: "failure" },
        ],
      };
      simulateResponse(channel, request.id, mockChecks);

      const result = await resultPromise;
      expect(result).toEqual(mockChecks);
    });
  });

  describe("RemoteProvider capabilities include canForge", () => {
    it("reports canForge: true when connected", async () => {
      const { provider } = createTestSetup();

      const caps = await provider.capabilities(TEST_REPO_ID);
      expect(caps.canForge).toBe(true);
    });

    it("reports canForge: false when reconnecting", async () => {
      const { provider, sessionManager } = createTestSetup();

      sessionManager.setStatus(TEST_PROFILE_ID, "reconnecting");

      const caps = await provider.capabilities(TEST_REPO_ID);
      expect(caps.canForge).toBe(false);
    });

    it("reports canForge: false when disconnected", async () => {
      const { provider, sessionManager } = createTestSetup();

      sessionManager.setStatus(TEST_PROFILE_ID, "disconnected");

      const caps = await provider.capabilities(TEST_REPO_ID);
      expect(caps.canForge).toBe(false);
    });
  });

  describe("PullRequestsView calls forge methods with repo_id (routing verification)", () => {
    it("forge methods accept repo_id as the routing key", () => {
      // The PullRequestsView calls rpc.forgeListPrs(repo.repo_id) which
      // translates to window.crab.rpc("forge_list_prs", { repo_id }).
      // The IPC handler extracts repo_id, resolves the provider via
      // ProviderRegistry, and dispatches through RemoteProvider.request().
      //
      // This test verifies the contract: forge methods include repo_id
      // in params, and parseRepoId correctly identifies remote repos.
      const remoteRepoId = "ssh://my-server/home/user/project";
      const parsed = parseRepoId(remoteRepoId);
      expect(parsed).not.toBeNull();
      expect(parsed!.profileId).toBe("my-server");

      // Local repo_ids are not parsed as remote
      expect(parseRepoId("/Users/user/project")).toBeNull();
    });
  });
});
