import { describe, expect, it } from "vitest";
import type { Workspace, WorkspaceRepoEntry } from "../src/state";
import {
  groupWorkspaceRepositories,
  matchesRepository,
  workspaceIdentity,
  workspaceRepoIdentity,
} from "../src/lib/workspace-sidebar";
import { isSidebarView, navigationGroup } from "../src/lib/navigation";

const entry = (
  relativePath: string,
  lastAccessedAt = 0,
): WorkspaceRepoEntry => ({
  name: relativePath.split("/").at(-1)!,
  relativePath,
  absolutePath: `/work/${relativePath}`,
  level: 1,
  branch: "main",
  dirtyCount: 0,
  ahead: 0,
  behind: 0,
  error: null,
  openRepoId: null,
  lastAccessedAt,
});
const workspace = (repos: WorkspaceRepoEntry[]): Workspace => ({
  kind: "local",
  rootPath: "/work",
  name: "work",
  repos,
  config: {
    version: 1,
    discovery: { maxDepth: 3, excludePatterns: [] },
    repos: { removed: [] },
    display: { tabBarState: [] },
  },
});

describe("workspace repository navigation", () => {
  it("partitions pins, five recent roots, and alphabetical remainder without duplicating worktrees", () => {
    const ws = workspace([
      entry("z"),
      entry("a"),
      entry("a/feature", 99),
      ...Array.from({ length: 7 }, (_, index) =>
        entry(`recent-${index}`, index + 1),
      ),
    ]);
    const result = groupWorkspaceRepositories(ws, new Set(["/work/z"]), {}, "");
    expect(
      result.groups.map((group) => [
        group.label,
        group.entries.map((repo) => repo.relativePath),
      ]),
    ).toEqual([
      ["Pinned", ["z"]],
      ["Recent", ["recent-6", "recent-5", "recent-4", "recent-3", "recent-2"]],
      ["All repositories", ["a", "recent-0", "recent-1"]],
    ]);
    expect(result.children.get("a")?.map((repo) => repo.relativePath)).toEqual([
      "a/feature",
    ]);
  });
  it("retains a parent when only its nested worktree matches", () => {
    const result = groupWorkspaceRepositories(
      workspace([entry("parent"), entry("parent/feature"), entry("other")]),
      new Set(),
      {},
      "FEATURE",
    );
    expect(
      result.groups.flatMap((group) =>
        group.entries.map((repo) => repo.relativePath),
      ),
    ).toEqual(["parent"]);
    expect(
      result.children.get("parent")?.map((repo) => repo.relativePath),
    ).toEqual(["parent/feature"]);
  });
  it("uses persisted history after discovery clears live access timestamps", () => {
    const result = groupWorkspaceRepositories(
      workspace([entry("a"), entry("b")]),
      new Set(),
      { b: 100 },
      "",
    );
    expect(result.groups[0]).toMatchObject({
      label: "Recent",
      entries: [{ relativePath: "b" }],
    });
  });
  it("keeps identical paths on different hosts separate from local pins", () => {
    const local = workspace([entry("same")]);
    const remote = { ...local, kind: "remote" as const, profileId: "host-a" };
    const other = { ...remote, profileId: "host-b" };
    const pins = new Set([workspaceRepoIdentity(remote, remote.repos[0])]);
    expect(
      [local, remote, other].map(
        (ws) => groupWorkspaceRepositories(ws, pins, {}, "").groups[0].label,
      ),
    ).toEqual(["All repositories", "Pinned", "All repositories"]);
    expect(new Set([local, remote, other].map(workspaceIdentity)).size).toBe(3);
  });
  it("matches standalone names and paths case-insensitively", () => {
    expect([
      matchesRepository(" SDK ", "sdk-client", "/work/lib"),
      matchesRepository("work", "sdk", "/Work/lib"),
      matchesRepository("missing", "sdk", "/work/lib"),
    ]).toEqual([true, true, false]);
  });
});

describe("canonical view ownership", () => {
  it("groups every Files and Git destination while retiring duplicate routes", () => {
    expect(
      [
        "explorer",
        "search",
        "changes",
        "timeline",
        "worktrees",
        "pull-requests",
      ].map((view) => (isSidebarView(view) ? navigationGroup(view) : null)),
    ).toEqual(["files", "files", "git", "git", "git", "git"]);
    expect(
      ["terminal", "branches", "conflicts", "health", "skills"].some(
        isSidebarView,
      ),
    ).toBe(false);
  });
});
