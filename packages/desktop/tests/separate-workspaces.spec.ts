import { describe, expect, it } from "vitest";
import { workspaceReducer } from "../src/lib/workspace-reducer";
import { entry, state, workspace } from "./fixtures/workspace";

// Transport and host identity belong to the workspace, not individual repos.
describe("separate local and remote workspaces", () => {
  for (const kind of ["local", "remote"] as const) {
    it(`preserves ${kind} ownership through discovery, status, promotion and removal`, () => {
      const owner = { ...workspace(), kind, ...(kind === "remote" ? { profileId: "server", profileLabel: "Server" } : {}) };
      let current = workspaceReducer(state(), { type: "workspace-set", workspace: owner });
      current = workspaceReducer(current, {
        type: "workspace-update-status", rootPath: owner.rootPath,
        statuses: [{ relativePath: "repo", branch: "main", dirtyCount: 0, ahead: 0, behind: 0, error: null }],
      });
      current = workspaceReducer(current, { type: "workspace-promote-l2", rootPath: owner.rootPath, relativePath: "repo", repoId: "opened" });
      current = workspaceReducer(current, { type: "workspace-remove-repo", rootPath: owner.rootPath, relativePath: "repo" });
      expect(current.workspaces[0]).toEqual({ ...owner, repos: [] });
    });
  }

  it("closing one remote workspace leaves the other host and local workspace intact", () => {
    const local = workspace();
    const remoteA = { ...workspace("/remote-a"), kind: "remote" as const, profileId: "a" };
    const remoteB = { ...workspace("/remote-b"), kind: "remote" as const, profileId: "b" };
    const result = workspaceReducer(state([local, remoteA, remoteB]), { type: "workspace-close", rootPath: remoteA.rootPath });
    expect(result.workspaces).toEqual([local, remoteB]);
  });

  it("re-discovery replaces only its root's repository inventory", () => {
    const local = workspace();
    const remote = { ...workspace("/remote", [entry("old")]), kind: "remote" as const, profileId: "server" };
    const discovered = { ...remote, repos: [entry("new")] };
    const result = workspaceReducer(state([local, remote]), { type: "workspace-set", workspace: discovered });
    expect(result.workspaces).toEqual([local, discovered]);
  });
});
