import { describe, it, expect } from "vitest";

import {
  TaskListenerRegistry,
  OrchestratorEventPayload,
} from "../electron/kanban/task-listener-registry";

describe("TaskListenerRegistry", () => {
  it("starts empty with size 0", () => {
    const registry = new TaskListenerRegistry();
    expect(registry.size).toBe(0);
  });

  it("register returns true and increments size on first registration", () => {
    const registry = new TaskListenerRegistry();
    const listener = (_payload: OrchestratorEventPayload) => {};

    const result = registry.register("task-1", listener);

    expect(result).toBe(true);
    expect(registry.size).toBe(1);
    expect(registry.has("task-1")).toBe(true);
  });

  it("register returns false (no-op) if listener already exists for that taskId", () => {
    const registry = new TaskListenerRegistry();
    const listener1 = (_payload: OrchestratorEventPayload) => {};
    const listener2 = (_payload: OrchestratorEventPayload) => {};

    registry.register("task-1", listener1);
    const result = registry.register("task-1", listener2);

    expect(result).toBe(false);
    expect(registry.size).toBe(1);
  });

  it("unregister returns true and decrements size when taskId exists", () => {
    const registry = new TaskListenerRegistry();
    registry.register("task-1", () => {});

    const result = registry.unregister("task-1");

    expect(result).toBe(true);
    expect(registry.size).toBe(0);
    expect(registry.has("task-1")).toBe(false);
  });

  it("unregister returns false when taskId does not exist", () => {
    const registry = new TaskListenerRegistry();

    const result = registry.unregister("nonexistent");

    expect(result).toBe(false);
  });

  it("has returns false for unregistered taskId", () => {
    const registry = new TaskListenerRegistry();

    expect(registry.has("task-1")).toBe(false);
  });

  it("supports multiple independent task listeners", () => {
    const registry = new TaskListenerRegistry();
    registry.register("task-1", () => {});
    registry.register("task-2", () => {});
    registry.register("task-3", () => {});

    expect(registry.size).toBe(3);
    expect(registry.has("task-1")).toBe(true);
    expect(registry.has("task-2")).toBe(true);
    expect(registry.has("task-3")).toBe(true);
  });

  it("unregistering one task does not affect others", () => {
    const registry = new TaskListenerRegistry();
    registry.register("task-1", () => {});
    registry.register("task-2", () => {});

    registry.unregister("task-1");

    expect(registry.size).toBe(1);
    expect(registry.has("task-1")).toBe(false);
    expect(registry.has("task-2")).toBe(true);
  });

  it("allows re-registration after unregister", () => {
    const registry = new TaskListenerRegistry();
    registry.register("task-1", () => {});
    registry.unregister("task-1");

    const result = registry.register("task-1", () => {});

    expect(result).toBe(true);
    expect(registry.size).toBe(1);
  });
});
