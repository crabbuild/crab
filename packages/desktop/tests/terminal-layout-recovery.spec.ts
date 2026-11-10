import { expect, it } from "vitest";
import { captureLayout, deserializeLayout, restoreLayoutTree } from "../src/lib/terminal-persistence";
import { createBranch, createLeaf, getAllLeaves } from "../src/lib/terminal-split-tree";

it("restores nested panes, tab order, sizes and active tabs after session IDs change", () => {
  const tab = (id: string) => ({ id, title: id, shell: "/bin/sh" });
  const a = createLeaf([tab("a"), tab("b")], "b");
  const b = createLeaf([tab("c")], "c");
  const c = createLeaf([tab("d")], "d");
  const original = createBranch("horizontal", [a, createBranch("vertical", [b, c], [30, 70])], [40, 60]);
  const saved = deserializeLayout(JSON.parse(JSON.stringify(captureLayout(original, c.id, () => "/repo"))))!;
  const restored = restoreLayoutTree(saved, new Map(["a", "b", "c", "d"].map(id => [id, tab(`${id}-live`)])));
  const leaves = getAllLeaves(restored.tree);
  expect(restored.tree).toMatchObject({ direction: "horizontal", sizes: [40, 60], children: [
    { id: a.id }, { direction: "vertical", sizes: [30, 70], children: [{ id: b.id }, { id: c.id }] },
  ] });
  expect(leaves.map(leaf => ({ ids: leaf.tabs.map(item => item.id), active: leaf.activeTab }))).toEqual([
    { ids: ["a-live", "b-live"], active: "b-live" },
    { ids: ["c-live"], active: "c-live" }, { ids: ["d-live"], active: "d-live" },
  ]);
  expect(restored.activePaneId).toBe(c.id);
});

it("saves an intentionally empty layout so hidden sessions do not reopen", () => {
  const leaf = createLeaf();
  const saved = deserializeLayout(captureLayout(leaf, leaf.id, () => "/repo"));
  expect(saved?.tabs).toEqual([]);
});

it("persists SSH identities and remote cwd without startup commands or credentials", () => {
  const remote = { id: "ssh-id", title: "remote", shell: "ssh", cwd: "/remote/project", sshRemote: { profileId: "profile", hostname: "host" }, startupCommand: "agent" };
  const leaf = createLeaf([remote], remote.id);
  const saved = deserializeLayout(captureLayout(leaf, leaf.id, tab => tab.cwd ?? ""))!;
  expect(saved.tabs[0]).toEqual({ id: "ssh-id", title: "remote", shell: "ssh", cwd: "/remote/project", sshRemote: { profileId: "profile", hostname: "host" } });
});

it("rejects split references that would lose or duplicate panes", () => {
  const a = createLeaf([{ id: "a", title: "a", shell: "sh" }], "a");
  const b = createLeaf([{ id: "b", title: "b", shell: "sh" }], "b");
  const saved = captureLayout(createBranch("horizontal", [a, b]), a.id, () => "/repo");
  saved.splits[0].children = [a.id, a.id];
  expect(deserializeLayout(saved)).toBeNull();
});
