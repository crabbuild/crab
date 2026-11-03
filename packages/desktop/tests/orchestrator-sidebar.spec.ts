import { describe, expect, it, vi, afterEach } from "vitest";
import {
  applyTaskFilter,
  buildTaskSidebarModel,
  dateBucket,
} from "../src/lib/orchestrator-sidebar";
import type { OrchestratorTask } from "../src/components/orchestrator/TaskCard";

function task(overrides: Partial<OrchestratorTask> & { id: string; title?: string }): OrchestratorTask {
  return {
    id: overrides.id,
    title: overrides.title ?? overrides.id,
    agentName: "codex",
    status: "reviewing",
    createdAt: 1_700_000_000_000,
    ...overrides,
  };
}

describe("orchestrator sidebar model", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("orders pinned, active, recent, and archived chats into separate sections", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-05-31T12:00:00Z"));

    const model = buildTaskSidebarModel([
      task({ id: "recent", title: "Recent done", status: "reviewing", lastActivityAt: Date.now() }),
      task({ id: "active", title: "Active work", status: "running", lastActivityAt: Date.now() - 1_000 }),
      task({ id: "pinned-old", title: "Pinned old", pinnedAt: 10 }),
      task({ id: "pinned-new", title: "Pinned new", pinnedAt: 20 }),
      task({ id: "archived", title: "Archived", status: "archived" }),
    ], { query: "", status: "all" });

    expect(model.pinnedTasks.map((t) => t.id)).toEqual(["pinned-new", "pinned-old"]);
    expect(model.activeTasks.map((t) => t.id)).toEqual(["active"]);
    expect(model.recentTasksByDate.Today.map((t) => t.id)).toContain("recent");
    expect(model.archivedTasks.map((t) => t.id)).toEqual(["archived"]);
  });

  it("nests child chats under their persisted parent", () => {
    const model = buildTaskSidebarModel([
      task({ id: "parent", title: "Parent" }),
      task({ id: "child", title: "Child", parentTaskId: "parent", rootTaskId: "parent", spawnedRole: "tests" }),
    ], { query: "", status: "all" });

    expect(model.childrenByParent.get("parent")?.map((t) => t.id)).toEqual(["child"]);
    expect(model.recentTasksByDate.Today.map((t) => t.id)).toEqual([]);
    expect(model.recentTasksByDate.Older.map((t) => t.id)).toEqual(["parent"]);
  });

  it("keeps parent context visible when only a child matches search", () => {
    const filtered = applyTaskFilter([
      task({ id: "parent", title: "Implement sidebar" }),
      task({ id: "child", title: "Audit keyboard navigation", parentTaskId: "parent", spawnedRole: "reviewer" }),
      task({ id: "other", title: "Unrelated" }),
    ], { query: "keyboard", status: "all" });

    expect(filtered.map((t) => t.id)).toEqual(["parent", "child"]);
  });

  it("searches sidebar metadata beyond the visible title", () => {
    const filtered = applyTaskFilter([
      task({ id: "a", title: "Plain title", branch: "feature/sidebar-control-center" }),
      task({ id: "b", title: "Other", sidebarGroup: "Release prep" }),
      task({ id: "c", title: "Pinned", pinnedAt: 123 }),
    ], { query: "sidebar-control", status: "all" });

    expect(filtered.map((t) => t.id)).toEqual(["a"]);
  });

  it("uses stable date buckets for recent chats", () => {
    const reference = new Date(2026, 4, 31, 12);

    expect(dateBucket(new Date(2026, 4, 31, 1).getTime(), reference)).toBe("Today");
    expect(dateBucket(new Date(2026, 4, 30, 12).getTime(), reference)).toBe("Yesterday");
    expect(dateBucket(new Date(2026, 4, 26, 12).getTime(), reference)).toBe("Last 7 days");
    expect(dateBucket(new Date(2026, 4, 1, 12).getTime(), reference)).toBe("Older");
  });
});
