import { describe, it, expect, vi } from "vitest";
import {
  reconcileOnStartup,
  parseSessionWorktreeId,
  SESSION_ID_SEPARATOR,
  type ReconcileDeps,
} from "../electron/daemon/startup-reconciliation";
import type { SessionInfo } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeSession(sessionId: string, isAlive = true): SessionInfo {
  return {
    sessionId,
    state: isAlive ? "running" : "exited",
    shellState: "ready",
    isAlive,
    pid: isAlive ? 12345 : null,
    cwd: "/tmp",
    cols: 80,
    rows: 24,
    createdAt: Date.now(),
  };
}

/** Mint a valid session ID from a worktreeId. */
function mintSessionId(worktreeId: string): string {
  return `${worktreeId}${SESSION_ID_SEPARATOR}abcd1234`;
}

function createMockDeps(
  sessions: SessionInfo[],
): { deps: ReconcileDeps; killSession: ReturnType<typeof vi.fn>; log: ReturnType<typeof vi.fn> } {
  const killSession = vi.fn().mockResolvedValue(undefined);
  const log = vi.fn();
  const deps: ReconcileDeps = {
    listSessions: vi.fn().mockResolvedValue(sessions),
    killSession,
    log,
  };
  return { deps, killSession, log };
}

// ---------------------------------------------------------------------------
// parseSessionWorktreeId
// ---------------------------------------------------------------------------

describe("parseSessionWorktreeId", () => {
  it("extracts worktreeId from a valid session ID", () => {
    const worktreeId = "repo-abc::/Users/dev/my-feature";
    const sessionId = `${worktreeId}@@a1b2c3d4`;

    expect(parseSessionWorktreeId(sessionId)).toBe(worktreeId);
  });

  it("handles worktreeId with hyphens and special characters", () => {
    const worktreeId = "my-repo-123::/home/user/projects/feature-branch";
    const sessionId = `${worktreeId}@@deadbeef`;

    expect(parseSessionWorktreeId(sessionId)).toBe(worktreeId);
  });

  it("returns null for bare UUID (no @@ separator)", () => {
    expect(parseSessionWorktreeId("550e8400-e29b-41d4-a716-446655440000")).toBeNull();
  });

  it("returns null for empty string", () => {
    expect(parseSessionWorktreeId("")).toBeNull();
  });

  it("returns null when @@ is at position 0", () => {
    expect(parseSessionWorktreeId("@@abcd1234")).toBeNull();
  });

  it("returns null when worktreeId has no :: separator", () => {
    // Has @@ but the left side doesn't contain ::
    expect(parseSessionWorktreeId("no-colons-here@@abcd1234")).toBeNull();
  });

  it("returns null when :: is at position 0 (empty repoId)", () => {
    expect(parseSessionWorktreeId("::/some/path@@abcd1234")).toBeNull();
  });

  it("returns null when :: is at end (empty path)", () => {
    expect(parseSessionWorktreeId("repo-id::@@abcd1234")).toBeNull();
  });

  it("uses lastIndexOf for @@ to handle @@ in worktreeId", () => {
    // Edge case: worktreeId itself contains @@
    const worktreeId = "repo@@special::/path/to/wt";
    const sessionId = `${worktreeId}@@shortid`;

    expect(parseSessionWorktreeId(sessionId)).toBe(worktreeId);
  });
});

// ---------------------------------------------------------------------------
// reconcileOnStartup
// ---------------------------------------------------------------------------

describe("reconcileOnStartup", () => {
  // -------------------------------------------------------------------------
  // Valid sessions preserved
  // -------------------------------------------------------------------------

  describe("valid sessions preserved", () => {
    it("preserves sessions whose worktree is in the valid set", async () => {
      const worktreeId = "repo-a::/wt/active";
      const sessionId = mintSessionId(worktreeId);
      const { deps, killSession } = createMockDeps([makeSession(sessionId)]);

      const result = await reconcileOnStartup(new Set([worktreeId]), deps);

      expect(result.alive).toEqual([sessionId]);
      expect(result.killed).toEqual([]);
      expect(killSession).not.toHaveBeenCalled();
    });

    it("preserves multiple sessions for the same valid worktree", async () => {
      const worktreeId = "repo-a::/wt/active";
      const session1 = `${worktreeId}@@aaaa1111`;
      const session2 = `${worktreeId}@@bbbb2222`;
      const { deps, killSession } = createMockDeps([
        makeSession(session1),
        makeSession(session2),
      ]);

      const result = await reconcileOnStartup(new Set([worktreeId]), deps);

      expect(result.alive).toEqual([session1, session2]);
      expect(result.killed).toEqual([]);
      expect(killSession).not.toHaveBeenCalled();
    });

    it("preserves sessions across multiple valid worktrees", async () => {
      const wt1 = "repo-a::/wt/one";
      const wt2 = "repo-b::/wt/two";
      const s1 = mintSessionId(wt1);
      const s2 = mintSessionId(wt2);
      const { deps } = createMockDeps([makeSession(s1), makeSession(s2)]);

      const result = await reconcileOnStartup(new Set([wt1, wt2]), deps);

      expect(result.alive).toEqual([s1, s2]);
      expect(result.killed).toEqual([]);
    });
  });

  // -------------------------------------------------------------------------
  // Orphan detection
  // -------------------------------------------------------------------------

  describe("orphan detection", () => {
    it("kills sessions whose worktree is not in the valid set", async () => {
      const orphanWt = "repo-a::/wt/deleted";
      const sessionId = mintSessionId(orphanWt);
      const { deps, killSession } = createMockDeps([makeSession(sessionId)]);

      const result = await reconcileOnStartup(new Set(["repo-a::/wt/other"]), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([sessionId]);
      expect(killSession).toHaveBeenCalledWith(sessionId);
    });

    it("kills orphans while preserving valid sessions", async () => {
      const validWt = "repo-a::/wt/keep";
      const orphanWt = "repo-a::/wt/removed";
      const validSession = mintSessionId(validWt);
      const orphanSession = mintSessionId(orphanWt);
      const { deps, killSession } = createMockDeps([
        makeSession(validSession),
        makeSession(orphanSession),
      ]);

      const result = await reconcileOnStartup(new Set([validWt]), deps);

      expect(result.alive).toEqual([validSession]);
      expect(result.killed).toEqual([orphanSession]);
      expect(killSession).toHaveBeenCalledWith(orphanSession);
      expect(killSession).toHaveBeenCalledTimes(1);
    });

    it("kills all sessions when valid set is empty", async () => {
      const s1 = mintSessionId("repo::/wt/a");
      const s2 = mintSessionId("repo::/wt/b");
      const { deps, killSession } = createMockDeps([makeSession(s1), makeSession(s2)]);

      const result = await reconcileOnStartup(new Set(), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([s1, s2]);
      expect(killSession).toHaveBeenCalledTimes(2);
    });
  });

  // -------------------------------------------------------------------------
  // Malformed ID handling
  // -------------------------------------------------------------------------

  describe("format validation", () => {
    it("kills sessions with bare UUID IDs (no @@ separator)", async () => {
      const bareUuid = "550e8400-e29b-41d4-a716-446655440000";
      const { deps, killSession } = createMockDeps([makeSession(bareUuid)]);

      const result = await reconcileOnStartup(new Set([bareUuid]), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([bareUuid]);
      expect(killSession).toHaveBeenCalledWith(bareUuid);
    });

    it("kills sessions with @@ but no :: in worktree portion", async () => {
      const malformed = "no-colons-here@@abcd1234";
      const { deps, killSession } = createMockDeps([makeSession(malformed)]);

      const result = await reconcileOnStartup(new Set(), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([malformed]);
      expect(killSession).toHaveBeenCalledWith(malformed);
    });

    it("kills sessions with empty repoId (:: at start)", async () => {
      const malformed = "::/some/path@@abcd1234";
      const { deps, killSession } = createMockDeps([makeSession(malformed)]);

      const result = await reconcileOnStartup(new Set(["::/some/path"]), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([malformed]);
      expect(killSession).toHaveBeenCalledWith(malformed);
    });

    it("kills sessions with empty path (:: at end of worktree portion)", async () => {
      const malformed = "repo-id::@@abcd1234";
      const { deps, killSession } = createMockDeps([makeSession(malformed)]);

      const result = await reconcileOnStartup(new Set(["repo-id::"]), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([malformed]);
      expect(killSession).toHaveBeenCalledWith(malformed);
    });
  });

  // -------------------------------------------------------------------------
  // Non-alive sessions skipped
  // -------------------------------------------------------------------------

  describe("non-alive sessions", () => {
    it("skips sessions that are not alive", async () => {
      const orphanWt = "repo::/wt/deleted";
      const sessionId = mintSessionId(orphanWt);
      const { deps, killSession } = createMockDeps([makeSession(sessionId, false)]);

      const result = await reconcileOnStartup(new Set(), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([]);
      expect(killSession).not.toHaveBeenCalled();
    });

    it("only processes alive sessions in a mixed list", async () => {
      const validWt = "repo::/wt/valid";
      const aliveSession = mintSessionId(validWt);
      const deadSession = `${validWt}@@dead0000`;
      const { deps } = createMockDeps([
        makeSession(aliveSession, true),
        makeSession(deadSession, false),
      ]);

      const result = await reconcileOnStartup(new Set([validWt]), deps);

      expect(result.alive).toEqual([aliveSession]);
      expect(result.killed).toEqual([]);
    });
  });

  // -------------------------------------------------------------------------
  // Error handling
  // -------------------------------------------------------------------------

  describe("error handling", () => {
    it("continues killing other sessions if one kill fails", async () => {
      const orphan1 = mintSessionId("repo::/wt/gone1");
      const orphan2 = mintSessionId("repo::/wt/gone2");
      const killSession = vi.fn()
        .mockRejectedValueOnce(new Error("already dead"))
        .mockResolvedValueOnce(undefined);
      const deps: ReconcileDeps = {
        listSessions: vi.fn().mockResolvedValue([
          makeSession(orphan1),
          makeSession(orphan2),
        ]),
        killSession,
      };

      const result = await reconcileOnStartup(new Set(), deps);

      expect(result.killed).toEqual([orphan1, orphan2]);
      expect(killSession).toHaveBeenCalledTimes(2);
    });

    it("returns empty results when no sessions exist", async () => {
      const { deps, killSession } = createMockDeps([]);

      const result = await reconcileOnStartup(new Set(["repo::/wt/valid"]), deps);

      expect(result.alive).toEqual([]);
      expect(result.killed).toEqual([]);
      expect(killSession).not.toHaveBeenCalled();
    });
  });
});
