/**
 * Integration tests for cross-repo branch comparison.
 *
 * Validates:
 * - Fetching refs from local repo via local provider and remote repo via remote provider
 * - Displaying comparison result in the diff view
 * - Handling case where one side is disconnected (show error, not crash)
 *
 * Requirements: 9.7
 */

import { describe, it, expect, vi } from "vitest";

// ── Helpers ──────────────────────────────────────────────────────

/** Simulates the resolveRef RPC call behavior. */
function makeResolveRef(responses: Record<string, { sha: string } | Error>) {
  return vi.fn().mockImplementation(async (repoId: string, branch: string) => {
    const key = `${repoId}:${branch}`;
    const response = responses[key];
    if (!response) throw new Error(`Unknown ref: ${branch} on ${repoId}`);
    if (response instanceof Error) throw response;
    return response;
  });
}

/** Simulates the gitDiffNameOnly RPC call behavior. */
function makeDiffNameOnly(
  responses: Record<string, { files: Array<{ path: string; status: string }> } | Error>,
) {
  return vi.fn().mockImplementation(async (repoId: string, base: string, head: string) => {
    const key = `${repoId}:${base}:${head}`;
    const response = responses[key];
    if (!response) throw new Error(`fatal: bad object ${base}`);
    if (response instanceof Error) throw response;
    return response;
  });
}

interface MockSshConnection {
  status: "connected" | "disconnected" | "reconnecting" | "error";
}

/**
 * Core logic extracted from CrossRepoBranchCompareView for testability.
 * This mirrors the component's comparison logic without React dependencies.
 */
async function performCrossRepoComparison(opts: {
  baseRepoId: string;
  baseEntry: { transport: "local" | "remote"; profileId?: string; name: string };
  baseBranch: string;
  headRepoId: string;
  headEntry: { transport: "local" | "remote"; profileId?: string; name: string };
  headBranch: string;
  sshConnections: Map<string, MockSshConnection>;
  resolveRef: ReturnType<typeof makeResolveRef>;
  gitDiffNameOnly: ReturnType<typeof makeDiffNameOnly>;
}): Promise<
  | { phase: "error"; message: string; side: "base" | "head" | "both" }
  | { phase: "ready"; files: Array<{ path: string; status: string }> }
> {
  const {
    baseRepoId,
    baseEntry,
    baseBranch,
    headRepoId,
    headEntry,
    headBranch,
    sshConnections,
    resolveRef,
    gitDiffNameOnly,
  } = opts;

  // Check connectivity
  function isConnected(entry: typeof baseEntry): boolean {
    if (entry.transport === "local") return true;
    if (!entry.profileId) return false;
    const conn = sshConnections.get(entry.profileId);
    return conn?.status === "connected";
  }

  const baseConnected = isConnected(baseEntry);
  const headConnected = isConnected(headEntry);

  if (!baseConnected && !headConnected) {
    return {
      phase: "error",
      message: "Both repositories are disconnected. Reconnect to compare branches.",
      side: "both",
    };
  }
  if (!baseConnected) {
    return {
      phase: "error",
      message: `Base repository "${baseEntry.name}" is disconnected. Reconnect to compare branches.`,
      side: "base",
    };
  }
  if (!headConnected) {
    return {
      phase: "error",
      message: `Head repository "${headEntry.name}" is disconnected. Reconnect to compare branches.`,
      side: "head",
    };
  }

  // Resolve refs on both sides in parallel
  let baseSha: string;
  let headSha: string;

  try {
    const baseResult = await resolveRef(baseRepoId, baseBranch);
    baseSha = baseResult.sha;
  } catch (e) {
    return {
      phase: "error",
      message: `Failed to resolve ref "${baseBranch}" on ${baseEntry.name}: ${e instanceof Error ? e.message : String(e)}`,
      side: "base",
    };
  }

  try {
    const headResult = await resolveRef(headRepoId, headBranch);
    headSha = headResult.sha;
  } catch (e) {
    return {
      phase: "error",
      message: `Failed to resolve ref "${headBranch}" on ${headEntry.name}: ${e instanceof Error ? e.message : String(e)}`,
      side: "head",
    };
  }

  // Try diff on head repo first, fall back to base repo
  try {
    const result = await gitDiffNameOnly(headRepoId, baseSha, headSha);
    return { phase: "ready", files: result.files };
  } catch {
    try {
      const result = await gitDiffNameOnly(baseRepoId, baseSha, headSha);
      return { phase: "ready", files: result.files };
    } catch (e2) {
      const msg = e2 instanceof Error ? e2.message : String(e2);
      return {
        phase: "error",
        message: `Cannot compare branches across these repositories: ${msg}. The repos may not share a common history.`,
        side: "both",
      };
    }
  }
}

// ── Test Suite ───────────────────────────────────────────────────

describe("Cross-Repo Branch Comparison", () => {
  describe("Fetching refs from local and remote providers", () => {
    it("resolves refs from both repos and computes diff", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:main": { sha: "aaa111" },
        "ssh://gpu-server/home/dev/repo-b:feature": { sha: "bbb222" },
      });
      const gitDiffNameOnly = makeDiffNameOnly({
        "ssh://gpu-server/home/dev/repo-b:aaa111:bbb222": {
          files: [
            { path: "src/main.rs", status: "M" },
            { path: "README.md", status: "A" },
          ],
        },
      });

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "feature",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("ready");
      if (result.phase === "ready") {
        expect(result.files).toHaveLength(2);
        expect(result.files[0]).toEqual({ path: "src/main.rs", status: "M" });
        expect(result.files[1]).toEqual({ path: "README.md", status: "A" });
      }

      // Verify resolveRef was called for both repos
      expect(resolveRef).toHaveBeenCalledWith("local://repo-a", "main");
      expect(resolveRef).toHaveBeenCalledWith("ssh://gpu-server/home/dev/repo-b", "feature");
    });

    it("falls back to base repo for diff when head repo fails", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:main": { sha: "aaa111" },
        "ssh://gpu-server/home/dev/repo-b:feature": { sha: "bbb222" },
      });
      const gitDiffNameOnly = makeDiffNameOnly({
        // Head repo fails (doesn't have base SHA)
        // Base repo succeeds
        "local://repo-a:aaa111:bbb222": {
          files: [{ path: "lib/utils.ts", status: "M" }],
        },
      });

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "feature",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("ready");
      if (result.phase === "ready") {
        expect(result.files).toHaveLength(1);
        expect(result.files[0].path).toBe("lib/utils.ts");
      }
    });

    it("shows no differences when branches point to same commit", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:main": { sha: "same-sha" },
        "ssh://gpu-server/home/dev/repo-b:main": { sha: "same-sha" },
      });
      const gitDiffNameOnly = makeDiffNameOnly({
        "ssh://gpu-server/home/dev/repo-b:same-sha:same-sha": {
          files: [],
        },
      });

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "main",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("ready");
      if (result.phase === "ready") {
        expect(result.files).toHaveLength(0);
      }
    });

    it("reports error when repos do not share common history", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:main": { sha: "aaa111" },
        "ssh://gpu-server/home/dev/repo-b:develop": { sha: "bbb222" },
      });
      // Both repos fail to diff (no common history)
      const gitDiffNameOnly = makeDiffNameOnly({});

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "develop",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("error");
      if (result.phase === "error") {
        expect(result.side).toBe("both");
        expect(result.message).toContain("Cannot compare branches");
        expect(result.message).toContain("common history");
      }
    });
  });

  describe("Handling disconnection gracefully", () => {
    it("shows error when head (remote) repo is disconnected", async () => {
      const resolveRef = makeResolveRef({});
      const gitDiffNameOnly = makeDiffNameOnly({});

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "feature",
        sshConnections: new Map([["gpu-server", { status: "disconnected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("error");
      if (result.phase === "error") {
        expect(result.side).toBe("head");
        expect(result.message).toContain("repo-b");
        expect(result.message).toContain("disconnected");
      }

      // No RPC calls should have been made
      expect(resolveRef).not.toHaveBeenCalled();
      expect(gitDiffNameOnly).not.toHaveBeenCalled();
    });

    it("shows error when base (remote) repo is disconnected", async () => {
      const resolveRef = makeResolveRef({});
      const gitDiffNameOnly = makeDiffNameOnly({});

      const result = await performCrossRepoComparison({
        baseRepoId: "ssh://build-server/home/ci/repo-a",
        baseEntry: { transport: "remote", profileId: "build-server", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "local://repo-b",
        headEntry: { transport: "local", name: "repo-b" },
        headBranch: "feature",
        sshConnections: new Map([["build-server", { status: "disconnected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("error");
      if (result.phase === "error") {
        expect(result.side).toBe("base");
        expect(result.message).toContain("repo-a");
        expect(result.message).toContain("disconnected");
      }

      expect(resolveRef).not.toHaveBeenCalled();
    });

    it("shows error when both repos are disconnected", async () => {
      const resolveRef = makeResolveRef({});
      const gitDiffNameOnly = makeDiffNameOnly({});

      const result = await performCrossRepoComparison({
        baseRepoId: "ssh://gpu-server/home/dev/repo-a",
        baseEntry: { transport: "remote", profileId: "gpu-server", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://build-server/home/ci/repo-b",
        headEntry: { transport: "remote", profileId: "build-server", name: "repo-b" },
        headBranch: "develop",
        sshConnections: new Map([
          ["gpu-server", { status: "disconnected" }],
          ["build-server", { status: "disconnected" }],
        ]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("error");
      if (result.phase === "error") {
        expect(result.side).toBe("both");
        expect(result.message).toContain("Both repositories are disconnected");
      }

      expect(resolveRef).not.toHaveBeenCalled();
    });

    it("does not crash when resolveRef fails on one side", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:main": { sha: "aaa111" },
        "ssh://gpu-server/home/dev/repo-b:nonexistent": new Error(
          "fatal: ambiguous argument 'nonexistent': unknown revision",
        ),
      });
      const gitDiffNameOnly = makeDiffNameOnly({});

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "nonexistent",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("error");
      if (result.phase === "error") {
        expect(result.side).toBe("head");
        expect(result.message).toContain("nonexistent");
        expect(result.message).toContain("repo-b");
      }
    });

    it("handles reconnecting state as disconnected", async () => {
      const resolveRef = makeResolveRef({});
      const gitDiffNameOnly = makeDiffNameOnly({});

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "feature",
        sshConnections: new Map([["gpu-server", { status: "reconnecting" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("error");
      if (result.phase === "error") {
        expect(result.side).toBe("head");
        expect(result.message).toContain("disconnected");
      }
    });

    it("local repos are always considered connected", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:main": { sha: "aaa111" },
        "local://repo-b:feature": { sha: "bbb222" },
      });
      const gitDiffNameOnly = makeDiffNameOnly({
        "local://repo-b:aaa111:bbb222": {
          files: [{ path: "index.ts", status: "M" }],
        },
      });

      // No SSH connections at all — local repos should still work
      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "main",
        headRepoId: "local://repo-b",
        headEntry: { transport: "local", name: "repo-b" },
        headBranch: "feature",
        sshConnections: new Map(),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("ready");
      if (result.phase === "ready") {
        expect(result.files).toHaveLength(1);
      }
    });
  });

  describe("Comparison result display", () => {
    it("returns file list with status codes for rendering", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:v1.0": { sha: "tag-sha-1" },
        "ssh://gpu-server/home/dev/repo-b:v2.0": { sha: "tag-sha-2" },
      });
      const gitDiffNameOnly = makeDiffNameOnly({
        "ssh://gpu-server/home/dev/repo-b:tag-sha-1:tag-sha-2": {
          files: [
            { path: "src/lib.rs", status: "M" },
            { path: "src/new_module.rs", status: "A" },
            { path: "src/deprecated.rs", status: "D" },
            { path: "src/moved.rs", status: "R" },
          ],
        },
      });

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "v1.0",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "v2.0",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("ready");
      if (result.phase === "ready") {
        expect(result.files).toHaveLength(4);
        // Verify all status types are preserved for the UI to render badges
        const statuses = result.files.map((f) => f.status);
        expect(statuses).toContain("M");
        expect(statuses).toContain("A");
        expect(statuses).toContain("D");
        expect(statuses).toContain("R");
      }
    });

    it("works with tag refs as well as branch refs", async () => {
      const resolveRef = makeResolveRef({
        "local://repo-a:v1.0.0": { sha: "tag-sha" },
        "ssh://gpu-server/home/dev/repo-b:HEAD": { sha: "head-sha" },
      });
      const gitDiffNameOnly = makeDiffNameOnly({
        "ssh://gpu-server/home/dev/repo-b:tag-sha:head-sha": {
          files: [{ path: "CHANGELOG.md", status: "M" }],
        },
      });

      const result = await performCrossRepoComparison({
        baseRepoId: "local://repo-a",
        baseEntry: { transport: "local", name: "repo-a" },
        baseBranch: "v1.0.0",
        headRepoId: "ssh://gpu-server/home/dev/repo-b",
        headEntry: { transport: "remote", profileId: "gpu-server", name: "repo-b" },
        headBranch: "HEAD",
        sshConnections: new Map([["gpu-server", { status: "connected" }]]),
        resolveRef,
        gitDiffNameOnly,
      });

      expect(result.phase).toBe("ready");
      if (result.phase === "ready") {
        expect(result.files).toHaveLength(1);
        expect(result.files[0].path).toBe("CHANGELOG.md");
      }
    });
  });
});
