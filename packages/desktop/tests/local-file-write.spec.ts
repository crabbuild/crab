import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { Agent } from "../electron/agent";
import { LocalProvider } from "../electron/local-provider";
import { captureFilePublications } from "../electron/file-write";

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

  it("status ownership captures an admitted temporary without reserving user filename prefixes", async () => {
    const foreign = path.join(root, ".crab-write-user-file");
    await fs.promises.writeFile(foreign, "USER");
    const entered = deferred(), release = deferred(), original = fs.promises.writeFile.bind(fs.promises);
    vi.spyOn(fs.promises, "writeFile").mockImplementationOnce(async (...args) => { await original(...args); entered.resolve(); await release.promise; });
    const snapshot = captureFilePublications(await fs.promises.realpath(root));
    const saving = provider.fsWrite("repo", file, "REPLACEMENT");
    let owned: Set<string>;
    try { await entered.promise; owned = await snapshot.finish(); }
    finally { release.resolve(); await saving; }
    expect([...owned!]).toHaveLength(1);
    expect(owned!.has(await fs.promises.realpath(foreign))).toBe(false);
    const completed = await captureFilePublications(await fs.promises.realpath(root)).finish();
    expect([...owned!].some(path => completed.has(path))).toBe(false);
  });

  it("exclusive-open failure never removes another owner's entry", async () => {
    const open = fs.promises.open.bind(fs.promises);
    let foreign: string | undefined;
    vi.spyOn(fs.promises, "open").mockImplementation(async (...args) => {
      if (args[1] === "wx") { foreign = String(args[0]); await fs.promises.writeFile(foreign, "FOREIGN"); }
      return open(...args);
    });
    const snapshot = captureFilePublications(await fs.promises.realpath(root));
    await expect(provider.fsWrite("repo", file, "REPLACEMENT")).rejects.toMatchObject({ code: "EEXIST" });
    expect((await snapshot.finish()).has(foreign!)).toBe(false);
    expect(await fs.promises.readFile(foreign!, "utf8")).toBe("FOREIGN");
    expect(await fs.promises.readFile(file, "utf8")).toBe("ORIGINAL");
  });

  it("a pending exclusive open cannot delay another worktree's status snapshot", async () => {
    const held = path.join(root, "held"), independent = path.join(root, "independent");
    await fs.promises.mkdir(held); await fs.promises.mkdir(independent);
    const entered = deferred(), release = deferred(), open = fs.promises.open.bind(fs.promises);
    vi.spyOn(fs.promises, "open").mockImplementation(async (...args) => {
      if (args[1] === "wx") { entered.resolve(); await release.promise; }
      return open(...args);
    });
    const saving = provider.fsWrite("held", path.join(held, "note.txt"), "HELD");
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await entered.promise;
      const paths = await Promise.race([
        captureFilePublications(await fs.promises.realpath(independent)).finish(),
        new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("Another worktree's acquisition blocked status")), 1000); }),
      ]);
      expect(paths.size).toBe(0);
    } finally { clearTimeout(timer); release.resolve(); await saving; }
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
