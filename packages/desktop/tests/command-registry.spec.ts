import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  registerCommand,
  unregisterCommand,
  getCommands,
  getCommand,
  filterCommands,
  executeCommand,
  subscribeRegistry,
  type Command,
  type CommandContext,
} from "../src/lib/command-registry";

// Mock the toasts module
vi.mock("../src/lib/toasts", () => ({
  publishToast: vi.fn(),
}));

// Mock the operation-history module
vi.mock("../src/lib/operation-history", () => ({
  recordOperation: vi.fn(),
}));

function makeContext(overrides: Partial<CommandContext> = {}): CommandContext {
  return {
    repoId: "test-repo",
    currentBranch: "main",
    upstream: "origin/main",
    ahead: 0,
    behind: 0,
    dirtyCount: 0,
    hasStagedChanges: false,
    activeOperation: null,
    forgeProvider: null,
    ...overrides,
  };
}

function makeCommand(overrides: Partial<Command> = {}): Command {
  return {
    id: "test.cmd",
    label: "Test Command",
    category: "Commit",
    isEnabled: () => true,
    execute: async () => ({ success: true, message: "done" }),
    ...overrides,
  };
}

// Reset registry state between tests by unregistering all commands
beforeEach(() => {
  for (const cmd of getCommands()) {
    unregisterCommand(cmd.id);
  }
});

describe("registerCommand / unregisterCommand", () => {
  it("registers a command and makes it retrievable", () => {
    const cmd = makeCommand({ id: "git.push", label: "Git: Push" });
    registerCommand(cmd);

    expect(getCommands()).toHaveLength(1);
    expect(getCommand("git.push")).toBeDefined();
    expect(getCommand("git.push")!.label).toBe("Git: Push");
  });

  it("replaces a command with the same id on re-registration", () => {
    registerCommand(makeCommand({ id: "git.push", label: "Push v1" }));
    registerCommand(makeCommand({ id: "git.push", label: "Push v2" }));

    expect(getCommands()).toHaveLength(1);
    expect(getCommand("git.push")!.label).toBe("Push v2");
  });

  it("unregisters a command by id", () => {
    registerCommand(makeCommand({ id: "git.push" }));
    registerCommand(makeCommand({ id: "git.pull" }));

    unregisterCommand("git.push");

    expect(getCommands()).toHaveLength(1);
    expect(getCommand("git.push")).toBeUndefined();
    expect(getCommand("git.pull")).toBeDefined();
  });

  it("unregistering a non-existent id is a no-op", () => {
    registerCommand(makeCommand({ id: "git.push" }));
    unregisterCommand("git.nonexistent");
    expect(getCommands()).toHaveLength(1);
  });
});

describe("getCommands / getCommand", () => {
  it("returns a copy of the commands array", () => {
    registerCommand(makeCommand({ id: "a" }));
    const cmds = getCommands();
    cmds.push(makeCommand({ id: "b" }));
    expect(getCommands()).toHaveLength(1);
  });

  it("getCommand returns undefined for unknown id", () => {
    expect(getCommand("nonexistent")).toBeUndefined();
  });
});

describe("filterCommands", () => {
  it("returns all enabled commands when query is empty", () => {
    registerCommand(makeCommand({ id: "a", label: "Alpha", isEnabled: () => true }));
    registerCommand(makeCommand({ id: "b", label: "Beta", isEnabled: () => true }));
    registerCommand(makeCommand({ id: "c", label: "Gamma", isEnabled: () => false }));

    const ctx = makeContext();
    const results = filterCommands("", ctx);

    expect(results).toHaveLength(2);
    expect(results.map((r) => r.id)).toContain("a");
    expect(results.map((r) => r.id)).toContain("b");
  });

  it("excludes disabled commands from results", () => {
    registerCommand(makeCommand({ id: "a", label: "Push", isEnabled: (ctx) => ctx.ahead > 0 }));
    registerCommand(makeCommand({ id: "b", label: "Pull", isEnabled: () => true }));

    const ctx = makeContext({ ahead: 0 });
    const results = filterCommands("", ctx);

    expect(results).toHaveLength(1);
    expect(results[0].id).toBe("b");
  });

  it("filters by substring match (fallback when fuzzy filter not loaded)", () => {
    registerCommand(makeCommand({ id: "a", label: "Git: Push", category: "Sync" }));
    registerCommand(makeCommand({ id: "b", label: "Git: Commit", category: "Commit" }));

    const ctx = makeContext();
    const results = filterCommands("push", ctx);

    expect(results).toHaveLength(1);
    expect(results[0].id).toBe("a");
  });

  it("matches against category as well", () => {
    registerCommand(makeCommand({ id: "a", label: "Git: Push", category: "Sync" }));
    registerCommand(makeCommand({ id: "b", label: "Git: Commit", category: "Commit" }));

    const ctx = makeContext();
    const results = filterCommands("commit", ctx);

    expect(results).toHaveLength(1);
    expect(results[0].id).toBe("b");
  });
});

describe("executeCommand", () => {
  it("does not announce failure or record work when confirmation is dismissed", async () => {
    const { publishToast } = await import("../src/lib/toasts");
    const { recordOperation } = await import("../src/lib/operation-history");
    vi.mocked(publishToast).mockClear();
    vi.mocked(recordOperation).mockClear();
    registerCommand(makeCommand({ execute: async () => ({ success: false, cancelled: true, message: "Cancelled" }) }));
    expect(await executeCommand("test.cmd", makeContext())).toEqual({ success: false, cancelled: true, message: "Cancelled" });
    expect(publishToast).not.toHaveBeenCalled();
    expect(recordOperation).not.toHaveBeenCalled();
  });

  it("reports a destructive result without inventing an Undo action", async () => {
    const { publishToast } = await import("../src/lib/toasts");
    const { recordOperation } = await import("../src/lib/operation-history");
    vi.mocked(publishToast).mockClear(); vi.mocked(recordOperation).mockClear();
    registerCommand(makeCommand({ destructive: true }));
    await executeCommand("test.cmd", makeContext());
    expect(vi.mocked(publishToast).mock.calls[0][0]).not.toHaveProperty("action");
    expect(vi.mocked(recordOperation).mock.calls[0][0]).not.toHaveProperty("undoId");
  });

  it("returns error for unknown command id", async () => {
    const result = await executeCommand("nonexistent", makeContext());
    expect(result.success).toBe(false);
    expect(result.message).toContain("Unknown command");
  });

  it("returns error when command is disabled", async () => {
    registerCommand(makeCommand({ id: "git.push", label: "Push", isEnabled: () => false }));

    const result = await executeCommand("git.push", makeContext());
    expect(result.success).toBe(false);
    expect(result.message).toContain("not available");
  });

  it("executes the command handler and returns its result", async () => {
    registerCommand(
      makeCommand({
        id: "git.push",
        execute: async () => ({ success: true, message: "Pushed to origin/main" }),
      }),
    );

    const result = await executeCommand("git.push", makeContext());
    expect(result.success).toBe(true);
    expect(result.message).toBe("Pushed to origin/main");
  });

  it("catches exceptions from the handler and returns error result", async () => {
    registerCommand(
      makeCommand({
        id: "git.push",
        execute: async () => {
          throw new Error("Network timeout");
        },
      }),
    );

    const result = await executeCommand("git.push", makeContext());
    expect(result.success).toBe(false);
    expect(result.message).toBe("Network timeout");
  });

  it("publishes a success toast on successful execution", async () => {
    const { publishToast } = await import("../src/lib/toasts");
    registerCommand(
      makeCommand({
        id: "git.push",
        label: "Git: Push",
        execute: async () => ({ success: true, message: "Pushed" }),
      }),
    );

    await executeCommand("git.push", makeContext());

    expect(publishToast).toHaveBeenCalledWith(
      expect.objectContaining({ kind: "success", title: "Git: Push" }),
    );
  });

  it("publishes an error toast on failed execution", async () => {
    const { publishToast } = await import("../src/lib/toasts");
    registerCommand(
      makeCommand({
        id: "git.push",
        label: "Git: Push",
        execute: async () => ({ success: false, message: "rejected" }),
      }),
    );

    await executeCommand("git.push", makeContext());

    expect(publishToast).toHaveBeenCalledWith(
      expect.objectContaining({ kind: "error", title: "Git: Push failed" }),
    );
  });

  it("passes input to the command handler", async () => {
    const executeFn = vi.fn(async (_ctx: CommandContext, input?: string) => ({
      success: true,
      message: `Checked out ${input}`,
    }));

    registerCommand(makeCommand({ id: "git.checkout", execute: executeFn }));

    await executeCommand("git.checkout", makeContext(), "feature/foo");

    expect(executeFn).toHaveBeenCalledWith(expect.anything(), "feature/foo");
  });
});

describe("pub-sub notifications", () => {
  it("notifies subscribers when a command is registered", () => {
    const callback = vi.fn();
    subscribeRegistry(callback);

    registerCommand(makeCommand({ id: "git.push" }));

    expect(callback).toHaveBeenCalledTimes(1);
    expect(callback).toHaveBeenCalledWith(
      expect.arrayContaining([expect.objectContaining({ id: "git.push" })]),
    );
  });

  it("notifies subscribers when a command is unregistered", () => {
    registerCommand(makeCommand({ id: "git.push" }));

    const callback = vi.fn();
    subscribeRegistry(callback);

    unregisterCommand("git.push");

    expect(callback).toHaveBeenCalledTimes(1);
    expect(callback).toHaveBeenCalledWith([]);
  });

  it("unsubscribe function stops notifications", () => {
    const callback = vi.fn();
    const unsub = subscribeRegistry(callback);

    unsub();
    registerCommand(makeCommand({ id: "git.push" }));

    expect(callback).not.toHaveBeenCalled();
  });
});
