import { beforeEach, describe, expect, it, vi } from "vitest";
import type { HydrationEntry } from "../src/ipc";
import type { OpenRepo } from "../src/state";
import { useFileTransfers } from "../src/hooks/useFileTransfers";

const native = vi.hoisted(() => ({ inventory: vi.fn(), status: vi.fn(), protect: vi.fn(), release: vi.fn(), confirm: vi.fn(), transfer: vi.fn() }));
vi.mock("react", () => ({ useMemo: (read: () => unknown) => read() }));
vi.mock("../src/state", () => ({ useStore: () => ({ runFileTransfer: native.transfer }), repositoryKey: (repo: OpenRepo) => repo.path ?? repo.repo_id }));
vi.mock("../src/ipc", () => ({ rpc: { hydrationStatus: native.inventory, gitStatus: native.status }, mainProcess: { dialogConfirm: native.confirm } }));

vi.mock("../src/lib/file-buffer-store", () => ({ fileBuffers: { protect: native.protect } }));

const repo = {
  repo_id: "ssh://profile/worktree", native_session_id: "captured-owner",
  capabilities: { canHydrateSelection: true, canDehydrateSelection: true, canRestoreSelection: true },
} as OpenRepo;
const entries: HydrationEntry[] = [
  { path: "model[1].bin", kind: "crab", state: "pointer", logical: 4096, on_disk: 128 },
  { path: "model1.bin", kind: "lfs", state: "pointer", logical: 8192, on_disk: 128 },
  { path: "saved.bin", kind: "lfs", state: "hydrated", logical: 2048, on_disk: 2048 },
  { path: "missing.bin", kind: "lfs", state: "missing", logical: 1024, on_disk: 0 },
];

beforeEach(() => {
  vi.resetAllMocks();
  native.protect.mockReturnValue(native.release);
  native.status.mockResolvedValue({ entries: [{ path: "model[1].bin", code: " M" }, { path: "model1.bin", code: "M " }, { path: "untracked", code: "??" }, { path: "both.bin", code: "MM" }, { path: "missing.bin", code: " D" }] });
  native.inventory.mockResolvedValue({ items: entries });
  native.confirm.mockResolvedValue(true);
  native.transfer.mockResolvedValue({ written_bytes: 4096 });
});

describe("verified selection entry points", () => {
  it("includes Crab and LFS missing/pointer content and skips hydrated files", async () => {
    expect(await useFileTransfers(repo).hydrate!()).toBe(3);
    expect(native.transfer).toHaveBeenCalledWith(repo.repo_id, { verb: "hydrate", paths: ["model[1].bin", "model1.bin", "missing.bin"] }, "captured-owner");
  });

  it("keeps exact bracket paths literal rather than interpreting them as patterns", async () => {
    await useFileTransfers(repo).hydrate!({ kind: "custom", paths: ["model[1].bin"] });
    expect(native.transfer).toHaveBeenCalledWith(repo.repo_id, { verb: "hydrate", paths: ["model[1].bin"] }, "captured-owner");
  });

  it("resolves a Settings pattern profile against the current native inventory", async () => {
    await useFileTransfers(repo).hydrate!({ kind: "patterns", patterns: ["model*.bin"] });
    expect(native.transfer).toHaveBeenCalledWith(repo.repo_id, { verb: "hydrate", paths: ["model[1].bin", "model1.bin"] }, "captured-owner");
  });

  it("confirms before dehydration and retains working files when dismissed", async () => {
    native.confirm.mockResolvedValue(false);
    expect(await useFileTransfers(repo).dehydrate!()).toBeNull();
    expect(native.confirm).toHaveBeenCalledWith(expect.stringContaining("Dehydrate 1 file"));
    expect(native.transfer).not.toHaveBeenCalled();
  });

  it.each([
    [1, 1024 ** 3],
    [100, 4096],
  ])("confirms hydration of %i files totaling %i bytes", async (count, total) => {
    native.inventory.mockResolvedValue({ items: Array.from({ length: count }, (_, index) => ({ ...entries[0], path: `${index}.bin`, logical: total / count })) });
    native.confirm.mockResolvedValue(false);
    expect(await useFileTransfers(repo).hydrate!()).toBeNull();
    expect(native.transfer).not.toHaveBeenCalled();
  });

  it("does not start or confirm work for an empty eligible selection", async () => {
    expect(await useFileTransfers(repo).dehydrate!({ kind: "custom", paths: ["model[1].bin"] })).toBe(0);
    expect(native.transfer).not.toHaveBeenCalled();
    expect(native.confirm).not.toHaveBeenCalled();
  });

  it("keeps the captured session after a confirmation outlives repository replacement", async () => {
    let confirm!: (value: boolean) => void;
    native.confirm.mockReturnValue(new Promise<boolean>(resolve => { confirm = resolve; }));
    const original = { ...repo };
    const pending = useFileTransfers(original).dehydrate!();
    await vi.waitFor(() => expect(native.confirm).toHaveBeenCalledOnce());
    original.native_session_id = "replacement";
    confirm(true);
    await pending;
    expect(native.transfer).toHaveBeenCalledWith(repo.repo_id, { verb: "dehydrate", paths: ["saved.bin"] }, "captured-owner");
  });

  it("propagates inventory and native publication failures without reporting a count", async () => {
    native.inventory.mockRejectedValueOnce(new Error("Inventory unavailable"));
    await expect(useFileTransfers(repo).hydrate!()).rejects.toThrow("Inventory unavailable");
    native.transfer.mockRejectedValueOnce(new Error("File changed"));
    await expect(useFileTransfers(repo).hydrate!()).rejects.toThrow("File changed");
  });

  it.each([null, { ...repo, native_session_id: null }, { ...repo, deferred: true }])("does not expose mutations for unavailable repositories", unavailable => {
    expect(useFileTransfers(unavailable)).toEqual({ hydrate: undefined, dehydrate: undefined, restore: undefined });
  });

  it("gates transfer directions independently and disables history mutations", () => {
    expect(useFileTransfers({ ...repo, capabilities: { ...repo.capabilities!, canDehydrateSelection: false } }).dehydrate).toBeUndefined();
    expect(useFileTransfers(repo, false)).toEqual({ hydrate: undefined, dehydrate: undefined, restore: undefined });
  });
});


describe("confirmed index restoration", () => {
  it("selects only unstaged tracked files and protects buffers through publication", async () => {
    expect(await useFileTransfers(repo).restore!()).toBe(3);
    expect(native.transfer).toHaveBeenCalledWith(repo.repo_id, { verb: "restore", paths: ["model[1].bin", "both.bin", "missing.bin"] }, "captured-owner");
    expect(native.protect).toHaveBeenCalledWith(["model[1].bin", "both.bin", "missing.bin"].map(path => JSON.stringify([repo.repo_id, path])));
    expect(native.release).toHaveBeenCalledOnce();
  });
  it("retains literal batch scope and excludes staged-only and untracked paths", async () => {
    await useFileTransfers(repo).restore!(["model[1].bin", "model[1].bin", "model1.bin", "untracked"]);
    expect(native.transfer).toHaveBeenCalledWith(repo.repo_id, { verb: "restore", paths: ["model[1].bin"] }, "captured-owner");
  });
  it.each(["dismissed", "failed"])("releases editor protection after %s restoration", async outcome => {
    if (outcome === "dismissed") {
      native.confirm.mockResolvedValue(false);
      expect(await useFileTransfers(repo).restore!()).toBeNull();
      expect(native.transfer).not.toHaveBeenCalled();
    } else {
      native.transfer.mockRejectedValue(new Error("Working file changed"));
      await expect(useFileTransfers(repo).restore!()).rejects.toThrow("Working file changed");
    }
    expect(native.release).toHaveBeenCalledOnce();
  });
  it("refuses pending editor buffers before confirmation or mutation", async () => {
    native.protect.mockImplementation(() => { throw new Error("Save pending"); });
    await expect(useFileTransfers(repo).restore!()).rejects.toThrow("Save pending");
    expect(native.confirm).not.toHaveBeenCalled();
    expect(native.transfer).not.toHaveBeenCalled();
  });
  it("does not expose restoration for an older runtime", () => {
    expect(useFileTransfers({ ...repo, capabilities: { ...repo.capabilities!, canRestoreSelection: false } }).restore).toBeUndefined();
  });
});
