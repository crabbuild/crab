import { describe, expect, it, vi } from "vitest";
import { executeCommand, getCommand, type CommandContext } from "../src/lib/command-registry";
import "../src/lib/view-commands";
import "../src/lib/git-commands";

vi.mock("../src/ipc", () => ({ rpc: {}, isStaleHunkError: () => false, isCancelledError: (error: { code?: number }) => error?.code === 1010 }));
vi.mock("../src/lib/toasts", () => ({ publishToast: vi.fn() }));
vi.mock("../src/lib/operation-history", () => ({ recordOperation: vi.fn() }));
const context: CommandContext = {
  repoId: "ssh://profile/repo", currentBranch: "main", upstream: null,
  ahead: 0, behind: 0, dirtyCount: 0, hasStagedChanges: false,
  activeOperation: null, forgeProvider: null,
};

describe("large-file command admission", () => {
  it.each(["hydrate", "dehydrate"])("runs %s through the available SSH selection owner", async verb => {
    const run = vi.fn().mockResolvedValue(2);
    const result = await executeCommand(`crab.${verb}`, { ...context, fileTransfers: { [verb]: run } });
    expect(result).toEqual({ success: true, message: `${verb === "hydrate" ? "Hydrated" : "Dehydrated"} 2 files` });
    expect(run).toHaveBeenCalledWith(undefined);
  });

  it("passes literal file intent through the same owner", async () => {
    const hydrate = vi.fn().mockResolvedValue(1);
    await executeCommand("crab.hydrate.file", { ...context, fileTransfers: { hydrate } }, "model[1].bin");
    expect(hydrate).toHaveBeenCalledWith({ kind: "custom", paths: ["model[1].bin"] });
  });

  it.each(["hydrate", "dehydrate"])("does not enable %s from a local path alone", verb => {
    expect(getCommand(`crab.${verb}`)!.isEnabled({ ...context, repoPath: "/unused/local/path" })).toBe(false);
  });

  it("retains a dismissed confirmation as cancellation rather than success", async () => {
    expect(await executeCommand("crab.dehydrate", { ...context, fileTransfers: { dehydrate: async () => null } })).toEqual({ success: false, cancelled: true, message: "File transfer cancelled" });
  });

  it("does not report acknowledged native cancellation as command failure", async () => {
    const hydrate = vi.fn().mockRejectedValue(Object.assign(new Error("Native cancellation"), { code: 1010 }));
    expect(await executeCommand("crab.hydrate", { ...context, fileTransfers: { hydrate } })).toEqual({ success: false, cancelled: true, message: "Operation cancelled" });
  });
});

it.each(["git.discard.file", "git.discard.all"])("routes %s through the verified restore owner", async id => {
  const restore = vi.fn().mockResolvedValue(1);
  const result = await executeCommand(id, { ...context, dirtyCount: 2, fileTransfers: { restore } }, "model[1].bin");
  expect(result.success).toBe(true);
  expect(restore.mock.calls).toEqual(id.endsWith("file") ? [[["model[1].bin"]]] : [[]]);
});
