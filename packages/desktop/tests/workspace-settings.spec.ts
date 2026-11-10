import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { readTextFile } from "../electron/file-read";
import { writeTextAtomically } from "../electron/file-write";
import { WorkspaceSettingsStore, documentTasks, workspaceOverrides } from "../src/lib/workspace-settings-store";

describe("shared workspace document", () => {
  let root: string, file: string, store: WorkspaceSettingsStore;
  beforeEach(async () => {
    root = await fs.mkdtemp(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "workspace-settings-"));
    file = path.join(root, "settings.json");
    store = new WorkspaceSettingsStore(target => readTextFile(target.fullPath, 64 * 1024), (target, content) => writeTextAtomically(target.fullPath, content));
    store.bind("workspace", { repoId: "workspace", fullPath: file, canEdit: true });
  });
  afterEach(async () => { vi.restoreAllMocks(); await fs.rm(root, { recursive: true, force: true }); });

  it("concurrent theme, settings and task changes preserve every acknowledged key", async () => {
    await fs.writeFile(file, JSON.stringify({ marker: { keep: true }, theme: "dark", tasks: [] }));
    const results = await Promise.all([
      store.update("workspace", document => { document.theme = "light"; }),
      store.update("workspace", document => { document.editorFontSize = 18; }),
      ...["first", "second"].map(id => store.update("workspace", document => {
        document.tasks = [...documentTasks(document), { id, title: id, done: false, createdAt: 1 }];
      })),
    ]);
    expect(results).toEqual([true, true, true, true]);
    expect(JSON.parse(await fs.readFile(file, "utf8"))).toEqual({ marker: { keep: true }, theme: "light", editorFontSize: 18,
      tasks: ["first", "second"].map(id => ({ id, title: id, done: false, createdAt: 1 })) });
  });

  it("only a missing document is a create case", async () => {
    expect(await store.update("workspace", document => { document.theme = "light"; })).toBe(true);
    expect(JSON.parse(await fs.readFile(file, "utf8"))).toEqual({ theme: "light" });
  });

  for (const [name, content] of [
    ["malformed", "{partial"], ["array", "[]"], ["invalid theme", '{"theme":"invalid theme"}'],
    ["invalid tasks", '{"tasks":[{"id":"one"}]}'], ["oversized", JSON.stringify({ marker: "x".repeat(64 * 1024) })],
  ]) it(`a ${name} document blocks mutation without replacing its bytes`, async () => {
    await fs.writeFile(file, content);
    expect(await store.update("workspace", document => { document.theme = "light"; })).toBe(false);
    expect(store.snapshot("workspace").error).toBeTruthy();
    expect(await fs.readFile(file, "utf8")).toBe(content);
  });

  it("an unavailable read retains acknowledged data and never creates an empty replacement", async () => {
    await fs.writeFile(file, '{"marker":"KEEP","theme":"dark"}');
    await store.load("workspace");
    const failure = Object.assign(new Error("Fixture read unavailable"), { code: "EIO" });
    vi.spyOn(fs, "open").mockRejectedValueOnce(failure);
    expect(await store.update("workspace", document => { document.theme = "light"; })).toBe(false);
    expect(store.snapshot("workspace")).toMatchObject({ document: { marker: "KEEP", theme: "dark" }, error: failure.message });
    expect(JSON.parse(await fs.readFile(file, "utf8"))).toEqual({ marker: "KEEP", theme: "dark" });
  });

  it("a denied save remains visible and retries from the current physical document", async () => {
    await fs.writeFile(file, '{"marker":"KEEP","theme":"dark"}');
    await store.load("workspace");
    vi.spyOn(fs, "rename").mockRejectedValueOnce(new Error("Fixture publication denied"));
    expect(await store.update("workspace", document => { document.theme = "light"; })).toBe(false);
    expect(store.snapshot("workspace").document?.theme).toBe("dark");
    expect(store.snapshot("workspace").error).toBe("Fixture publication denied");
    await fs.writeFile(file, '{"marker":"EXTERNAL_CHANGE","theme":"dark"}');
    expect(await store.update("workspace", document => { document.theme = "light"; })).toBe(true);
    expect(JSON.parse(await fs.readFile(file, "utf8"))).toEqual({ marker: "EXTERNAL_CHANGE", theme: "light" });
  });

  it("a replacement connection cannot publish a document read by its previous owner", async () => {
    let release!: () => void, entered!: () => void;
    const gate = new Promise<void>(resolve => { release = resolve; }), ready = new Promise<void>(resolve => { entered = resolve; });
    const write = vi.fn(async () => 1);
    const owned = new WorkspaceSettingsStore(async () => { entered(); await gate; return { exists: true, content: '{"theme":"dark"}' }; }, write);
    owned.bind("remote", { repoId: "remote", fullPath: "/repo/settings.json", canEdit: true, owner: "first" });
    const save = owned.update("remote", document => { document.theme = "light"; });
    await ready;
    expect(owned.pendingCount()).toBe(1);
    owned.bind("remote", { repoId: "remote", fullPath: "/repo/settings.json", canEdit: true, owner: "replacement" });
    release();
    expect(await save).toBe(false);
    expect(write).not.toHaveBeenCalled();
    expect(owned.pendingCount()).toBe(0);
  });

  it("read-only bindings reject mutation before any physical read or write", async () => {
    const read = vi.fn(), write = vi.fn();
    const readonly = new WorkspaceSettingsStore(read, write);
    readonly.bind("snapshot", { repoId: "snapshot", fullPath: "/snapshot/settings.json", canEdit: false });
    expect(await readonly.update("snapshot", document => { document.theme = "light"; })).toBe(false);
    expect(read).not.toHaveBeenCalled(); expect(write).not.toHaveBeenCalled();
  });

  it("a held repository read does not block another repository's document", async () => {
    let release!: () => void;
    const gate = new Promise<void>(resolve => { release = resolve; });
    const independent = new WorkspaceSettingsStore(async target => {
      if (target.repoId === "held") await gate;
      return readTextFile(target.fullPath, 64 * 1024);
    }, (target, content) => writeTextAtomically(target.fullPath, content));
    const heldFile = path.join(root, "held.json");
    independent.bind("held", { repoId: "held", fullPath: heldFile, canEdit: true });
    independent.bind("ready", { repoId: "ready", fullPath: file, canEdit: true });
    const held = independent.update("held", document => { document.theme = "dark"; });
    try {
      expect(await independent.update("ready", document => { document.theme = "light"; })).toBe(true);
      expect(await fs.readFile(file, "utf8")).toContain('"light"');
    } finally { release(); await held; }
    expect(await fs.readFile(heldFile, "utf8")).toContain('"dark"');
  });

  it("repository documents cannot override device execution or credential settings", () => {
    expect(workspaceOverrides({ theme: "light", editorFontSize: 18, orchestratorClaudeCodePath: "/untrusted/agent", telemetryOptIn: true, aiEndpoint: "https://untrusted.invalid" })).toEqual({ theme: "light", editorFontSize: 18 });
  });

  it("physical reads reject oversize and invalid UTF-8 rather than returning a partial document", async () => {
    for (const bytes of [Buffer.alloc(65), Buffer.from([0xff])]) {
      await fs.writeFile(file, bytes);
      await expect(readTextFile(file, 64)).rejects.toThrow();
    }
  });
});
