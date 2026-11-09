/**
 * Verification tests for remote git operations over the SSH router.
 *
 * These tests verify that the RPC Router correctly forwards all git
 * verbs to the remote agent. Since the router transparently forwards
 * any method, these tests confirm the routing logic works for the
 * full set of git operations.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import { RpcRouter, parseRepoId } from "../electron/ssh-router";

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

function createRouter(): { router: RpcRouter; channel: MockChannel; sessionManager: MockSessionManager; localAgent: MockLocalAgent } {
  const sessionManager = new MockSessionManager() as any;
  const localAgent = new MockLocalAgent();
  const router = new RpcRouter(sessionManager, localAgent, { requestTimeoutMs: 5000 });
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

// ─── Tests ───────────────────────────────────────────────────────

describe("Remote Git Operations via RPC Router", () => {
  let router: RpcRouter;
  let channel: MockChannel;
  let localAgent: MockLocalAgent;

  beforeEach(() => {
    const setup = createRouter();
    router = setup.router;
    channel = setup.channel;
    localAgent = setup.localAgent;
  });

  describe("git status and diff verbs", () => {
    it("routes git_status_porcelain to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/ml-repo";
      const promise = router.route(repoId, "git_status_porcelain", { repo_id: repoId });

      // Simulate response
      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { entries: [{ path: "model.py", status: "M" }] });

      const result = await promise;
      expect(result).toEqual({ entries: [{ path: "model.py", status: "M" }] });
      expect(written.method).toBe("git_status_porcelain");
    });

    it("routes file_diff to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "file_diff", { repo_id: repoId, path: "src/main.rs" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { diff: "@@ -1,3 +1,4 @@\n+new line" });

      const result = await promise;
      expect(result).toEqual({ diff: "@@ -1,3 +1,4 @@\n+new line" });
    });

    it("routes git_log to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_log", { repo_id: repoId, limit: 10 });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { commits: [{ sha: "abc123", message: "init" }] });

      const result = await promise;
      expect(result).toEqual({ commits: [{ sha: "abc123", message: "init" }] });
    });
  });

  describe("git write operations", () => {
    it("routes git_commit to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_commit", { repo_id: repoId, message: "feat: add model" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { sha: "def456" });

      const result = await promise;
      expect(result).toEqual({ sha: "def456" });
      expect(written.method).toBe("git_commit");
    });

    it("routes git_stage_hunks to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_stage_hunks", { repo_id: repoId, hunks: [] });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { staged: true });

      const result = await promise;
      expect(result).toEqual({ staged: true });
    });

    it("routes git_unstage_hunks to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const hunks = [{ path: "src/main.rs", start: 10, count: 5 }];
      const promise = router.route(repoId, "git_unstage_hunks", { repo_id: repoId, hunks });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("git_unstage_hunks");
      expect(written.params.hunks).toEqual(hunks);
      simulateResponse(channel, written.id, { unstaged: 1 });

      const result = await promise;
      expect(result).toEqual({ unstaged: 1 });
    });

    it("routes git_discard_hunks to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const hunks = [{ path: "lib/utils.ts", start: 20, count: 3 }];
      const promise = router.route(repoId, "git_discard_hunks", { repo_id: repoId, hunks });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("git_discard_hunks");
      expect(written.params.hunks).toEqual(hunks);
      simulateResponse(channel, written.id, { discarded: 1 });

      const result = await promise;
      expect(result).toEqual({ discarded: 1 });
    });

    it("routes hunk operations with multiple hunks to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const hunks = [
        { path: "src/main.rs", start: 10, count: 5 },
        { path: "src/main.rs", start: 30, count: 8 },
        { path: "src/lib.rs", start: 1, count: 12 },
      ];
      const promise = router.route(repoId, "git_stage_hunks", { repo_id: repoId, hunks });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("git_stage_hunks");
      expect(written.params.hunks).toHaveLength(3);
      simulateResponse(channel, written.id, { staged: 3 });

      const result = await promise;
      expect(result).toEqual({ staged: 3 });
    });

    it("routes git_checkout to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_checkout", { repo_id: repoId, ref: "feature-branch" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { success: true });

      const result = await promise;
      expect(result).toEqual({ success: true });
    });
  });

  describe("branch operations", () => {
    it("routes git_branch_create to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_branch_create", { repo_id: repoId, name: "new-branch" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { created: "new-branch" });

      const result = await promise;
      expect(result).toEqual({ created: "new-branch" });
    });

    it("routes git_branch_delete to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_branch_delete", { repo_id: repoId, name: "old-branch" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { deleted: true });

      const result = await promise;
      expect(result).toEqual({ deleted: true });
    });
  });

  describe("fetch/pull/push operations", () => {
    it("routes git_fetch_all to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_fetch_all", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { fetched: true });

      const result = await promise;
      expect(result).toEqual({ fetched: true });
    });

    it("routes git_pull_branch to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_pull_branch", { repo_id: repoId, branch: "main" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { pulled: true });

      const result = await promise;
      expect(result).toEqual({ pulled: true });
    });

    it("routes git_push_branch to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_push_branch", { repo_id: repoId, branch: "main" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { pushed: true });

      const result = await promise;
      expect(result).toEqual({ pushed: true });
    });
  });

  describe("merge/rebase/cherry-pick operations", () => {
    it("routes op_start to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "op_start", { repo_id: repoId, op: "merge", target: "feature" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { op_id: "op-123" });

      const result = await promise;
      expect(result).toEqual({ op_id: "op-123" });
    });

    it("routes op_continue to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "op_continue", { repo_id: repoId, op_id: "op-123" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { completed: true });

      const result = await promise;
      expect(result).toEqual({ completed: true });
    });

    it("routes op_abort to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "op_abort", { repo_id: repoId, op_id: "op-123" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { aborted: true });

      const result = await promise;
      expect(result).toEqual({ aborted: true });
    });
  });

  describe("stash operations", () => {
    it("routes git_stash_list to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_stash_list", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { stashes: [{ index: 0, message: "WIP" }] });

      const result = await promise;
      expect(result).toEqual({ stashes: [{ index: 0, message: "WIP" }] });
    });
  });

  describe("search operations", () => {
    it("routes git_grep to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_grep", { repo_id: repoId, pattern: "TODO" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { matches: [{ file: "main.rs", line: 42 }] });

      const result = await promise;
      expect(result).toEqual({ matches: [{ file: "main.rs", line: 42 }] });
    });
  });

  describe("file preview operations", () => {
    it("routes read_text to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "read_text", { repo_id: repoId, path: "README.md" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { content: "# Hello" });

      const result = await promise;
      expect(result).toEqual({ content: "# Hello" });
    });

    it("routes preview_parquet to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "preview_parquet", { repo_id: repoId, path: "data.parquet" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { columns: ["id", "name"], rows: 1000 });

      const result = await promise;
      expect(result).toEqual({ columns: ["id", "name"], rows: 1000 });
    });
  });

  describe("large response handling", () => {
    it("handles large diff responses without truncation at router level", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const largeDiff = "a".repeat(10_000_000); // 10MB
      const promise = router.route(repoId, "file_diff", { repo_id: repoId, path: "big.rs" });

      const written = JSON.parse(channel.written[0]);
      simulateResponse(channel, written.id, { diff: largeDiff });

      const result = await promise;
      expect((result as { diff: string }).diff.length).toBe(10_000_000);
    });
  });

  describe("inline patch diff transport-agnosticism", () => {
    it("diff patch content from remote is identical text format to local", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      // Simulate a unified diff patch — the same format returned by both
      // local and remote agents (git diff output is text, transport-agnostic).
      const patchContent = [
        "diff --git a/src/main.rs b/src/main.rs",
        "index abc1234..def5678 100644",
        "--- a/src/main.rs",
        "+++ b/src/main.rs",
        "@@ -10,6 +10,7 @@ fn main() {",
        "     let config = Config::load();",
        "     let engine = Engine::new(&config);",
        "+    engine.set_remote(true);",
        "     engine.run();",
        " }",
      ].join("\n");

      const promise = router.route(repoId, "git_diff_index_patch", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("git_diff_index_patch");
      simulateResponse(channel, written.id, { patch: patchContent });

      const result = await promise;
      // The patch text is returned verbatim — no transport-specific
      // transformation. InlinePatchDiff renders this identically
      // regardless of whether it came from a local or remote agent.
      expect((result as { patch: string }).patch).toBe(patchContent);
      expect((result as { patch: string }).patch).toContain("@@ -10,6 +10,7 @@");
      expect((result as { patch: string }).patch).toContain("+    engine.set_remote(true);");
    });

    it("cached patch from remote preserves hunk boundaries for stage/unstage", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const cachedPatch = [
        "diff --git a/lib/utils.ts b/lib/utils.ts",
        "index 1111111..2222222 100644",
        "--- a/lib/utils.ts",
        "+++ b/lib/utils.ts",
        "@@ -1,4 +1,5 @@",
        " export function add(a: number, b: number) {",
        "+  console.log('adding', a, b);",
        "   return a + b;",
        " }",
        "@@ -20,3 +21,4 @@",
        " export function multiply(a: number, b: number) {",
        "+  console.log('multiplying', a, b);",
        "   return a * b;",
        " }",
      ].join("\n");

      const promise = router.route(repoId, "git_diff_cached_patch", { repo_id: repoId });

      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("git_diff_cached_patch");
      simulateResponse(channel, written.id, { patch: cachedPatch });

      const result = await promise;
      // Two hunks are present — the renderer can parse and render each
      // independently for hunk-level unstage/discard operations.
      const patch = (result as { patch: string }).patch;
      const hunkHeaders = patch.match(/^@@.*@@/gm);
      expect(hunkHeaders).toHaveLength(2);
    });
  });

  describe("routing correctness", () => {
    it("routes local repos to local agent", async () => {
      const localRepoId = "/home/user/local-repo";
      await router.route(localRepoId, "git_status_porcelain", { repo_id: localRepoId });

      expect(localAgent.calls).toHaveLength(1);
      expect(localAgent.calls[0].method).toBe("git_status_porcelain");
    });

    it("routes ssh:// repos to remote agent", async () => {
      const repoId = "ssh://gpu-server/home/user/repo";
      const promise = router.route(repoId, "git_status_porcelain", { repo_id: repoId });

      expect(channel.written).toHaveLength(1);
      const written = JSON.parse(channel.written[0]);
      expect(written.method).toBe("git_status_porcelain");

      simulateResponse(channel, written.id, { entries: [] });
      await promise;
    });

    it("parseRepoId correctly identifies remote repos", () => {
      expect(parseRepoId("ssh://gpu-server/home/user/repo")).toEqual({
        profileId: "gpu-server",
        remotePath: "/home/user/repo",
      });
      expect(parseRepoId("/local/path")).toBeNull();
      expect(parseRepoId("https://github.com/repo")).toBeNull();
    });
  });
});
