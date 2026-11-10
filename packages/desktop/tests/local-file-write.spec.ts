import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { Agent } from "../electron/agent";
import { LocalProvider } from "../electron/local-provider";

vi.mock("electron", () => ({ shell: { trashItem: vi.fn() } }));

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>(yes => { resolve = yes; });
  return { promise, resolve };
}

describe("local repository file publication", () => {
  let root: string, file: string, provider: LocalProvider;
  beforeEach(async () => {
    root = await fs.promises.mkdtemp(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "local-file-write-"));
    file = path.join(root, "note.txt");
    await fs.promises.writeFile(file, "ORIGINAL", { mode: 0o751 });
    provider = new LocalProvider(new Agent({ agent: "unused-agent", crab: "unused-crab" }));
  });
  afterEach(async () => {
    vi.restoreAllMocks(); provider.dispose();
    await fs.promises.rm(root, { recursive: true, force: true });
  });

  it("a partial write failure retains the original and leaves no temporary", async () => {
    const original = fs.promises.writeFile.bind(fs.promises);
    const failure = Object.assign(new Error("Fixture disk write failed"), { code: "EIO" });
    vi.spyOn(fs.promises, "writeFile").mockImplementationOnce(async file => {
      await original(file, "PARTIAL", "utf8");
      throw failure;
    });
    await expect(provider.fsWrite("repo", file, "REPLACEMENT")).rejects.toBe(failure);
    expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
    expect(await fs.promises.readdir(root)).toEqual(["note.txt"]);
  });

  it("readers retain the original until the admitted write completes", async () => {
    const original = fs.promises.writeFile.bind(fs.promises), entered = deferred(), release = deferred();
    vi.spyOn(fs.promises, "writeFile").mockImplementationOnce(async (...args) => {
      await original(...args); entered.resolve(); await release.promise;
    });
    const saving = provider.fsWrite("repo", file, "REPLACEMENT");
    try {
      await entered.promise;
      expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
    } finally { release.resolve(); await saving; }
    expect(await fs.promises.readFile(file, "utf8")).toBe("REPLACEMENT");
  });

  it("an already-open source retains its original bytes after publication", async () => {
    const captured = await fs.promises.open(file, "r");
    try {
      await provider.fsWrite("repo", file, "REPLACEMENT");
      expect(await captured.readFile("utf8")).toBe("ORIGINAL");
    } finally { await captured.close(); }
  });

  it("a failed rename retains the target and removes the prepared temporary", async () => {
    const failure = Object.assign(new Error("Fixture publication failed"), { code: "EIO" });
    vi.spyOn(fs.promises, "rename").mockRejectedValueOnce(failure);
    await expect(provider.fsWrite("repo", file, "REPLACEMENT")).rejects.toBe(failure);
    expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
    expect(await fs.promises.readdir(root)).toEqual(["note.txt"]);
  });

  it("cleanup failure preserves both errors without changing the target", async () => {
    const primary = new Error("Fixture publication failed"), cleanup = new Error("Fixture cleanup failed");
    vi.spyOn(fs.promises, "rename").mockRejectedValueOnce(primary);
    vi.spyOn(fs.promises, "unlink").mockRejectedValueOnce(cleanup);
    await expect(provider.fsWrite("repo", file, "REPLACEMENT")).rejects.toMatchObject({ errors: [primary, cleanup], cause: primary });
    expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
  });

  it("exclusive-open failure never removes another owner's entry", async () => {
    const open = fs.promises.open.bind(fs.promises);
    let foreign: string | undefined;
    vi.spyOn(fs.promises, "open").mockImplementation(async (...args) => {
      if (args[1] === "wx") { foreign = String(args[0]); await fs.promises.writeFile(foreign, "FOREIGN"); }
      return open(...args);
    });
    await expect(provider.fsWrite("repo", file, "REPLACEMENT")).rejects.toMatchObject({ code: "EEXIST" });
    expect(await fs.promises.readFile(foreign!, "utf8")).toBe("FOREIGN");
    expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
  });

  it("empty and UTF-8 saves acknowledge persisted bytes without native services", async () => {
    const results = [];
    for (const content of ["", "🦀 café"]) {
      results.push(await provider.fsWrite("repo", file, content));
      expect(await fs.promises.readFile(file, "utf8")).toBe(content);
    }
    expect(results.map(result => result.written)).toEqual([0, Buffer.byteLength("🦀 café")]);
  });

  it("read-only targets remain unchanged", async () => {
    const original = (await fs.promises.stat(file)).mode;
    await fs.promises.chmod(file, 0o444);
    try {
      await expect(provider.fsWrite("repo", file, "REPLACEMENT")).rejects.toMatchObject({ code: "EACCES" });
      expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
    } finally { await fs.promises.chmod(file, original & 0o7777); }
  });

  it("repository URLs cannot become local filesystem writes", async () => {
    const resolve = vi.spyOn(fs.promises, "realpath");
    for (const target of ["ssh://profile/repository/settings.json", "s3://bucket/repository/settings.json", "relative.txt"]) {
      await expect(provider.fsWrite("repo", target, "REPLACEMENT")).rejects.toMatchObject({ code: "EINVAL" });
    }
    expect(resolve).not.toHaveBeenCalled();
  });

  it.skipIf(process.platform === "win32")("preserves existing mode bits", async () => {
    await provider.fsWrite("repo", file, "REPLACEMENT");
    expect((await fs.promises.stat(file)).mode & 0o7777).toBe(0o751);
  });

  it.skipIf(process.platform === "win32")("retains internal links and rejects dangling links without replacement", async () => {
    const link = path.join(root, "link.txt"), dangling = path.join(root, "dangling.txt");
    await fs.promises.symlink(file, link); await fs.promises.symlink("missing.txt", dangling);
    await provider.fsWrite("repo", link, "REPLACEMENT");
    expect((await fs.promises.lstat(link)).isSymbolicLink()).toBe(true);
    expect(await fs.promises.readFile(file, "utf8")).toBe("REPLACEMENT");
    await expect(provider.fsWrite("repo", dangling, "REPLACEMENT")).rejects.toMatchObject({ code: "ENOENT" });
    expect((await fs.promises.lstat(dangling)).isSymbolicLink()).toBe(true);
  });
});
