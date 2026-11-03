import { describe, expect, it } from "vitest";
import {
  pathBelongsToRepo,
  rendererIdFromDaemonSessionId,
  sessionBelongsToRepoPath,
  toDaemonSessionSummary,
  type ReattachCandidateSession,
} from "../electron/terminal-reattach";

function session(overrides: Partial<ReattachCandidateSession>): ReattachCandidateSession {
  return {
    sessionId: "other@@term-d-1",
    isAlive: true,
    initialCwd: null,
    cwd: null,
    ...overrides,
  };
}

describe("terminal reattach matching", () => {
  it("matches sessions by renderer session id prefix", () => {
    expect(sessionBelongsToRepoPath(
      session({ sessionId: "/repo/work@@term-d-3" }),
      "/repo/work",
    )).toBe(true);
  });

  it("matches task sessions by stable initial cwd after shell cd", () => {
    expect(sessionBelongsToRepoPath(
      session({
        sessionId: "/workspace/root@@term-d-3",
        initialCwd: "/repo/.crab/worktrees/task-a",
        cwd: "/tmp",
      }),
      "/repo/.crab/worktrees/task-a",
    )).toBe(true);
  });

  it("matches nested cwd paths within a task worktree", () => {
    expect(pathBelongsToRepo(
      "/repo/.crab/worktrees/task-a/src",
      "/repo/.crab/worktrees/task-a",
    )).toBe(true);
  });

  it("does not match sibling worktree names with the same prefix", () => {
    expect(pathBelongsToRepo(
      "/repo/.crab/worktrees/task-ab",
      "/repo/.crab/worktrees/task-a",
    )).toBe(false);
  });

  it("does not match dead sessions", () => {
    expect(sessionBelongsToRepoPath(
      session({
        isAlive: false,
        initialCwd: "/repo/.crab/worktrees/task-a",
      }),
      "/repo/.crab/worktrees/task-a",
    )).toBe(false);
  });

  it("derives the renderer id from repo-prefixed daemon session ids", () => {
    expect(rendererIdFromDaemonSessionId("/repo/work@@term-d-12", "/repo/work")).toBe("term-d-12");
  });

  it("derives the renderer id from worktree daemon session ids outside the selected repo prefix", () => {
    expect(rendererIdFromDaemonSessionId("/repo/root@@term-d-7", "/repo/.crab/worktrees/task-a")).toBe("term-d-7");
  });

  it("builds metadata-only daemon summaries without attaching sessions", () => {
    const summary = toDaemonSessionSummary({
      sessionId: "/repo/work@@term-d-4",
      state: "running",
      shellState: "ready",
      isAlive: true,
      pid: 123,
      initialCwd: "/repo/work",
      cwd: "/repo/work/src",
      cols: 100,
      rows: 40,
      createdAt: 1000,
      lastDataAt: 2000,
    }, "/repo/work");

    expect(summary).toMatchObject({
      id: "term-d-4",
      daemonSessionId: "/repo/work@@term-d-4",
      cwd: "/repo/work/src",
      lastDataAt: 2000,
    });
  });
});
