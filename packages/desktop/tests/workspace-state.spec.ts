import { describe, expect, it } from "vitest";
import { workspaceReducer } from "../src/lib/workspace-reducer";
import { entry, state, workspace } from "./fixtures/workspace";

// StoreProvider delegates these actions to this same reducer.
describe("workspace state", () => {
  it("opens distinct workspaces without replacing the others", () => {
    const local = workspace();
    const remote = { ...workspace("/remote"), kind: "remote" as const, profileId: "server" };
    const opened = workspaceReducer(state([local]), { type: "workspace-set", workspace: remote });
    expect(opened.workspaces).toEqual([local, remote]);
  });

  it("replaces discovery results for an existing root without duplicating it", () => {
    const refreshed = workspace("/workspace", [entry("new")]);
    const result = workspaceReducer(state([workspace()]), { type: "workspace-set", workspace: refreshed });
    expect(result.workspaces).toEqual([refreshed]);
  });

  it("selects a workspace independently of repository tabs", () => {
    const original = { ...state([workspace(), workspace("/other")]), tabBar: ["repo"] };
    const result = workspaceReducer(original, { type: "set-active-workspace", rootPath: "/other" });
    expect(result).toEqual({ ...original, activeWorkspaceRootPath: "/other" });
  });

  it("closing the selected workspace selects the remaining one", () => {
    const result = workspaceReducer(state([workspace(), workspace("/other")]), { type: "workspace-close", rootPath: "/workspace" });
    expect(result.activeWorkspaceRootPath).toBe("/other");
  });

  it("closing another workspace preserves the selection", () => {
    const result = workspaceReducer(state([workspace(), workspace("/other")]), { type: "workspace-close", rootPath: "/other" });
    expect(result.activeWorkspaceRootPath).toBe("/workspace");
  });

  it("closing all workspaces clears their selection and tabs", () => {
    const result = workspaceReducer({ ...state([workspace()]), tabBar: ["repo"] }, { type: "workspace-close" });
    expect(result).toEqual(state());
  });

  it("status updates promote repositories to L1 without changing other roots", () => {
    const original = state([workspace(), workspace("/other")]);
    const result = workspaceReducer(original, {
      type: "workspace-update-status", rootPath: "/workspace",
      statuses: [{ relativePath: "repo", branch: "main", dirtyCount: 2, ahead: 1, behind: 0, error: null }],
    });
    expect(result.workspaces).toEqual([
      workspace("/workspace", [entry("repo", { level: 1, branch: "main", dirtyCount: 2, ahead: 1, behind: 0 })]),
      original.workspaces[1],
    ]);
  });

  it("status failures preserve the previous loading level and repository data", () => {
    const original = entry("repo", { level: 1, branch: "main", dirtyCount: 2 });
    const result = workspaceReducer(state([workspace("/workspace", [original])]), {
      type: "workspace-update-status", rootPath: "/workspace",
      statuses: [{ relativePath: "repo", branch: null, dirtyCount: 0, ahead: null, behind: null, error: "disconnected" }],
    });
    expect(result.workspaces[0].repos[0]).toEqual({ ...original, error: "disconnected" });
  });

  it("status polling retains L2 repository handles and clears recovered errors", () => {
    const original = entry("repo", { level: 2, openRepoId: "opened", error: "disconnected" });
    const result = workspaceReducer(state([workspace("/workspace", [original])]), {
      type: "workspace-update-status", rootPath: "/workspace",
      statuses: [{ relativePath: "repo", branch: "main", dirtyCount: 0, ahead: 0, behind: 0, error: null }],
    });
    expect(result.workspaces[0].repos[0]).toEqual({ ...original, branch: "main", dirtyCount: 0, ahead: 0, behind: 0, error: null });
  });

  it("promotion opens only the repository in the requested root", () => {
    const original = state([workspace(), workspace("/other")]);
    const result = workspaceReducer(original, { type: "workspace-promote-l2", rootPath: "/workspace", relativePath: "repo", repoId: "opened" });
    expect(result.workspaces).toEqual([
      workspace("/workspace", [entry("repo", { level: 2, openRepoId: "opened" })]), original.workspaces[1],
    ]);
  });

  it("promotion preserves an existing L2 handle", () => {
    const opened = entry("repo", { level: 2, openRepoId: "original" });
    const result = workspaceReducer(state([workspace("/workspace", [opened])]), { type: "workspace-promote-l2", rootPath: "/workspace", relativePath: "repo", repoId: "replacement" });
    expect(result.workspaces[0].repos[0]).toBe(opened);
  });

  it("removes a repository from its workspace and tab bar", () => {
    const original = { ...state([workspace("/workspace", [entry("repo"), entry("other")])]), tabBar: ["repo", "other"] };
    const result = workspaceReducer(original, { type: "workspace-remove-repo", rootPath: "/workspace", relativePath: "repo" });
    expect(result).toEqual({ ...original, workspaces: [workspace("/workspace", [entry("other")])], tabBar: ["other"] });
  });

  it("activating an existing tab preserves its position", () => {
    const original = { ...state([workspace()]), tabBar: ["first", "repo", "last"] };
    expect(workspaceReducer(original, { type: "tab-bar-activate", relativePath: "repo" }).tabBar).toEqual(original.tabBar);
  });

  it("opening a tenth tab evicts the oldest tab", () => {
    const original = { ...state([workspace()]), tabBar: Array.from({ length: 9 }, (_, index) => `repo-${index}`) };
    expect(workspaceReducer(original, { type: "tab-bar-activate", relativePath: "new" }).tabBar).toEqual([...original.tabBar.slice(1), "new"]);
  });

  it("closing a tab retains the discovered repository", () => {
    const original = { ...state([workspace()]), tabBar: ["repo"] };
    expect(workspaceReducer(original, { type: "tab-bar-close", relativePath: "repo" })).toEqual({ ...original, tabBar: [] });
  });
});
