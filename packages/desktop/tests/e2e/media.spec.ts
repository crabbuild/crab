import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, openSync, ftruncateSync, closeSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";

function sparseWav(file: string, size: number) {
  const header = Buffer.alloc(44);
  header.write("RIFF"); header.writeUInt32LE(size - 8, 4); header.write("WAVEfmt ", 8);
  header.writeUInt32LE(16, 16); header.writeUInt16LE(1, 20); header.writeUInt16LE(2, 22);
  header.writeUInt32LE(48000, 24); header.writeUInt32LE(192000, 28);
  header.writeUInt16LE(4, 32); header.writeUInt16LE(16, 34); header.write("data", 36); header.writeUInt32LE(size - 44, 40);
  writeFileSync(file, header);
  const descriptor = openSync(file, "r+");
  try { ftruncateSync(descriptor, size); } finally { closeSync(descriptor); }
}

function sparseWebm(file: string) {
  const bytes = readFileSync(path.resolve(__dirname, "../fixtures/media/seek.webm"));
  const segment = bytes.indexOf(Buffer.from([0x18, 0x53, 0x80, 0x67]));
  expect(segment).toBeGreaterThan(0);
  expect(bytes[segment + 4]).toBe(1); // ffmpeg writes the Segment length as an eight-byte VINT.
  const padding = 240 * 1024 ** 2;
  bytes.writeBigUInt64BE((1n << 56n) | BigInt(bytes.length - segment - 12 + padding), segment + 4);
  const voidHeader = Buffer.alloc(9);
  voidHeader[0] = 0xec;
  voidHeader.writeBigUInt64BE((1n << 56n) | BigInt(padding - 9), 1);
  writeFileSync(file, Buffer.concat([bytes, voidHeader]));
  const descriptor = openSync(file, "r+");
  try { ftruncateSync(descriptor, bytes.length + padding); } finally { closeSync(descriptor); }
}

test("packaged media seeks large Crab and LFS files through retained byte ranges", async () => {
  test.setTimeout(180_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-media-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    mkdirSync(path.join(source, ".crab"));
    writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks").replaceAll("\\", "/"))}\n`);
    const driver = `'${crab.replaceAll("\\", "/").replaceAll("'", "'\\''")}' filter-process`;
    for (const kind of ["crab", "lfs"]) {
      git(["config", `filter.${kind}.process`, driver]);
      git(["config", `filter.${kind}.required`, "true"]);
    }
    writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
    writeFileSync(path.join(source, ".gitattributes"), "crab.wav filter=crab -text\nlfs.wav filter=lfs -text\nmovie.webm filter=crab -text\n");
    sparseWav(path.join(source, "crab.wav"), 320 * 1024 ** 2);
    sparseWav(path.join(source, "lfs.wav"), 320 * 1024 ** 2);
    sparseWebm(path.join(source, "movie.webm"));
    sparseWav(path.join(source, "git.wav"), 4 * 192000 + 44);
    const pointers = new Map<string, Buffer>();
    for (const file of ["crab.wav", "lfs.wav", "movie.webm"]) {
      git(["add", file]);
      const pointer = git(["show", `:${file}`]);
      expect(pointer.length).toBeLessThan(1024);
      pointers.set(file, pointer);
      writeFileSync(path.join(source, file), pointer);
    }
    git(["add", ".gitignore", ".gitattributes", "git.wav"]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Captured media fixture"]);
    application = await electron.launch({ executablePath, env, args: [`--user-data-dir=${path.join(root, "profile")}`] });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBeTruthy();
    // Observe production agent calls without substituting data or bypassing IPC.
    await application.evaluate(({ app }) => {
      type Entry = { method: string; path?: unknown; file_id?: unknown; offset?: unknown; length?: unknown };
      const evidence = globalThis as typeof globalThis & { mediaEvidence: Entry[] };
      evidence.mediaEvidence = [];
      const load = process.getBuiltinModule("module").createRequire(app.getAppPath() + "/package.json");
      const { Agent } = load("./dist-electron/agent.cjs") as { Agent: { prototype: { request: (method: string, params: Record<string, unknown>) => Promise<unknown> } } };
      const original = Agent.prototype.request;
      Agent.prototype.request = async function (method, params) {
        const result = await original.call(this, method, params);
        if (params.repo_id === "media" && ["read_bytes", "read_range", "file_open", "file_close"].includes(method)) {
          const opened = result as { file_id?: unknown };
          evidence.mediaEvidence.push({ method, path: params.path, file_id: opened?.file_id ?? params.file_id, offset: params.offset, length: params.length });
        }
        return result;
      };
    });
    const handle = await call(page, "open_local", { repo_id: "media", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    let previous: string | null = null;
    for (const file of ["crab.wav", "lfs.wav", "movie.webm", "git.wav"]) {
      await page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
      const element = page.locator(file.endsWith("webm") ? "video" : "audio");
      await expect(element).toBeVisible();
      await expect.poll(() => element.evaluate((node: HTMLMediaElement) => Number.isFinite(node.duration) && node.duration > 0)).toBe(true);
      const url = await element.getAttribute("src");
      expect(url).toMatch(/^crab-content:\/\/media\/[0-9a-f-]+$/);
      if (previous) await expect.poll(() => page.evaluate(async url => (await fetch(url, { method: "HEAD" })).status, previous!)).toBe(410);
      previous = url;
      await element.evaluate((node: HTMLMediaElement) => { node.muted = true; node.currentTime = node.duration * 0.8; });
      await expect.poll(() => element.evaluate((node: HTMLMediaElement) => !node.seeking && Math.abs(node.currentTime - node.duration * 0.8) < 0.1)).toBe(true);
      await element.evaluate((node: HTMLMediaElement) => node.play());
      await expect.poll(() => element.evaluate((node: HTMLMediaElement) => node.currentTime > node.duration * 0.8 + 0.1)).toBe(true);
      await element.evaluate((node: HTMLMediaElement) => node.pause());
      if (pointers.has(file)) {
        expect(readFileSync(path.join(source, file))).toEqual(pointers.get(file));
        writeFileSync(path.join(source, file), pointers.get(file === "movie.webm" ? "lfs.wav" : "movie.webm")!);
        const bytes = await page.evaluate(async url => {
          const response = await fetch(url, { headers: { Range: "bytes=0-3" } });
          return { status: response.status, range: response.headers.get("Content-Range"), bytes: Array.from(new Uint8Array(await response.arrayBuffer())) };
        }, url!);
        expect(bytes).toMatchObject({ status: 206, bytes: file.endsWith("webm") ? [0x1a, 0x45, 0xdf, 0xa3] : [82, 73, 70, 70] });
        writeFileSync(path.join(source, file), pointers.get(file)!);
      }
      await page.screenshot({ path: test.info().outputPath(`${file}.png`) });
    }
    const pointer = pointers.get("lfs.wav")!.toString();
    const oid = /^oid sha256:([0-9a-f]{64})$/m.exec(pointer)![1];
    const payload = path.join(source, ".git/lfs/objects", oid.slice(0, 2), oid.slice(2, 4), oid);
    const descriptor = openSync(payload, "r+");
    try { ftruncateSync(descriptor, 1); } finally { closeSync(descriptor); }
    await page.locator('[role="treeitem"][data-item-path="lfs.wav"]').click();
    await expect(page.getByRole("alert").filter({ hasText: /native content read failed/i })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("media-integrity-error.png") });
    await call(page, "close", { repo_id: "media" });
    const evidence = await application.evaluate(() => (globalThis as typeof globalThis & { mediaEvidence: { method: string; file_id?: string; offset?: number; length?: number }[] }).mediaEvidence);
    const ranges = evidence.filter(entry => entry.method === "read_range");
    expect(ranges.length).toBeGreaterThan(4);
    expect(ranges.every(entry => entry.length! <= 1024 ** 2)).toBe(true);
    expect(ranges.some(entry => entry.offset! > 200 * 1024 ** 2)).toBe(true);
    expect(evidence.filter(entry => entry.method === "read_bytes")).toHaveLength(0);
    const captures = evidence.filter(entry => entry.method === "file_open").map(entry => entry.file_id).sort();
    const closed = evidence.filter(entry => entry.method === "file_close").map(entry => entry.file_id).sort();
    expect(closed).toEqual(captures);
    const evidencePath = test.info().outputPath("native-media-reads.json");
    writeFileSync(evidencePath, JSON.stringify(evidence, null, 2) + "\n");
    await test.info().attach("native-media-reads", { path: evidencePath, contentType: "application/json" });
    for (const [file, pointer] of pointers) expect(readFileSync(path.join(source, file))).toEqual(pointer);
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
