import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, openSync, ftruncateSync, closeSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";

const desktopVersion: string = JSON.parse(readFileSync(path.resolve(__dirname, "../../package.json"), "utf8")).version;

test("packaged desktop previews large pointers and materializes staged Crab content", async () => {
  test.setTimeout(240_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-large-files-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR;
  delete env.GIT_WORK_TREE;
  delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  const pointers = new Map<string, Buffer>();
  let stagedContent: Buffer | undefined;
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    mkdirSync(path.join(source, ".crab"), { recursive: true });
    writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks").replaceAll("\\", "/"))}\n`);
    const driver = `'${crab.replaceAll("\\", "/").replaceAll("'", "'\\''")}' filter-process`;
    for (const kind of ["crab", "lfs"]) {
      git(["config", `filter.${kind}.process`, driver]);
      git(["config", `filter.${kind}.required`, "true"]);
    }
    writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
    writeFileSync(path.join(source, ".gitattributes"), "large.safetensors filter=crab -text\nstaged.safetensors filter=crab -text\nlfs.safetensors filter=lfs -text\n");
    const sizes = { "large.safetensors": 3 * 1024 ** 3, "lfs.safetensors": 4 * 1024 ** 2, "staged.safetensors": 4 * 1024 ** 2 };
    for (const [file, size] of Object.entries(sizes)) {
      const header = Buffer.from(JSON.stringify({ weights: { dtype: "U8", shape: [size], data_offsets: [0, size] } }));
      const prefix = Buffer.alloc(8);
      prefix.writeBigUInt64LE(BigInt(header.length));
      if (file === "staged.safetensors") stagedContent = Buffer.concat([prefix, header, Buffer.alloc(size)]);
      writeFileSync(path.join(source, file), Buffer.concat([prefix, header]));
      const descriptor = openSync(path.join(source, file), "r+");
      try { ftruncateSync(descriptor, prefix.length + header.length + size); }
      finally { closeSync(descriptor); }
      git(["add", file]);
      const pointer = git(["show", `:${file}`]);
      expect(pointer.length).toBeLessThan(1024);
      pointers.set(file, pointer);
      writeFileSync(path.join(source, file), pointer);
    }
    git(["add", ".gitattributes", ".gitignore"]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Captured large-file fixture"]);
    application = await electron.launch({ executablePath, env, args: [`--user-data-dir=${path.join(root, "profile")}`] });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBe(desktopVersion);
    const handle = await call(page, "open_local", { repo_id: "large-files", path: source });
    await page.evaluate(({ handle, source }) => {
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } }));
    }, { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    for (const [file, size] of Object.entries(sizes)) {
      await page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
      await expect(page.getByText("Safetensors · ML weights", { exact: true })).toBeVisible();
      await expect(page.getByRole("cell", { name: "weights", exact: true })).toBeVisible();
      const metadata = await call(page, "preview_safetensors", { repo_id: "large-files", path: file, rev: "HEAD" });
      expect((metadata.tensors as { nbytes: number }[])[0].nbytes).toBe(size);
      const captured = await call(page, "file_open", { repo_id: "large-files", path: file });
      const range = await call(page, "read_range", { repo_id: "large-files", file_id: captured.file_id, offset: size - 16, length: 16 });
      expect(Buffer.from(range.bytes_b64 as string, "base64")).toEqual(Buffer.alloc(16));
      await call(page, "file_close", { repo_id: "large-files", file_id: captured.file_id });
      expect(readFileSync(path.join(source, file))).toEqual(pointers.get(file));
      await page.screenshot({ path: test.info().outputPath(`${file}.png`) });
    }
    await page.locator('[role="treeitem"][data-item-path="large.safetensors"]').click();
    await page.getByRole("button", { name: "File details", exact: true }).click();
    const inspector = page.getByRole("region", { name: "File details", exact: true });
    await expect(inspector.getByText("Crab chunks", { exact: true })).toBeVisible();
    await inspector.getByRole("button", { name: "Inspect chunks", exact: true }).click();
    const chunks = inspector.getByRole("table", { name: "File chunks", exact: true });
    await expect(chunks.getByRole("row")).toHaveCount(65);
    await expect(chunks.locator("tbody tr").first().getByRole("cell").first()).toHaveText("1");
    // A later pointer replacement must not redirect an already captured recipe.
    writeFileSync(path.join(source, "large.safetensors"), pointers.get("lfs.safetensors")!);
    await inspector.getByRole("button", { name: "Next chunks", exact: true }).click();
    await expect(chunks.locator("tbody tr").first().getByRole("cell").first()).toHaveText("65");
    await expect(inspector.getByText("Crab chunks", { exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("captured-chunk-page.png") });
    await inspector.getByRole("button", { name: "Previous chunks", exact: true }).click();
    await expect(chunks.locator("tbody tr").first().getByRole("cell").first()).toHaveText("1");
    writeFileSync(path.join(source, "large.safetensors"), pointers.get("large.safetensors")!);
    await page.getByRole("button", { name: "Back to preview", exact: true }).click();
    await expect(page.getByRole("cell", { name: "weights", exact: true })).toBeVisible();
    const opened = await call(page, "file_open", { repo_id: "large-files", path: "large.safetensors" });
    const first = await call(page, "file_chunks", { repo_id: "large-files", file_id: opened.file_id, start: 0, limit: 1 });
    expect(first.chunk_count as number).toBeGreaterThan(64);
    expect(first.xorb_count).toBeNull();
    expect((first.chunks as Array<{ offset: number; hash: string }>)[0]).toMatchObject({ offset: 0, hash: expect.stringMatching(/^[0-9a-f]{64}$/) });
    await call(page, "file_close", { repo_id: "large-files", file_id: opened.file_id });
    const pointer = pointers.get("lfs.safetensors")!.toString();
    const oid = /^oid sha256:([0-9a-f]{64})$/m.exec(pointer)![1];
    const payload = path.join(source, ".git", "lfs", "objects", oid.slice(0, 2), oid.slice(2, 4), oid);
    const original = readFileSync(payload);
    const corrupt = Buffer.from(original);
    corrupt[corrupt.length - 1] ^= 1;
    writeFileSync(payload, corrupt);
    await page.locator('[role="treeitem"][data-item-path="large.safetensors"]').click();
    await page.locator('[role="treeitem"][data-item-path="lfs.safetensors"]').click();
    await expect(page.getByText(/native content read failed/i)).toBeVisible();
    await expect(page.getByText(/native content read failed/i)).not.toContainText("Error invoking remote method");
    const failure = await page.evaluate(async () => {
      const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<unknown> } }).crab;
      try {
        await bridge.rpc("preview_safetensors", { repo_id: "large-files", path: "lfs.safetensors" });
        return null;
      } catch (error) {
        const detail = error as { code?: number; message?: string; data?: unknown };
        return { code: detail.code, message: detail.message, data: detail.data };
      }
    });
    expect(failure).toMatchObject({ code: 1012, data: { kind: "content_read" } });
    await page.screenshot({ path: test.info().outputPath("lfs-integrity-error.png") });
    writeFileSync(payload, original);
    await page.locator('[role="treeitem"][data-item-path="large.safetensors"]').click();
    await page.locator('[role="treeitem"][data-item-path="lfs.safetensors"]').click();
    await expect(page.getByRole("cell", { name: "weights", exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(pointers.get("lfs.safetensors"));
    const cachePayload = path.join(root, "cache/chunks/aa", "a".repeat(64));
    const retained = path.join(root, "cache/retained-workspace/user-file");
    for (const file of [cachePayload, retained]) mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
    writeFileSync(cachePayload, "data", { mode: 0o600 });
    writeFileSync(retained, "keep", { mode: 0o600 });
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    const cache = page.getByRole("region", { name: "Cache usage", exact: true });
    await expect(cache.getByRole("button", { name: "Refresh usage", exact: true })).toBeEnabled();
    await cache.getByRole("button", { name: "Preview cleanup", exact: true }).click();
    await expect(cache.getByRole("status")).toContainText("Would remove");
    expect(existsSync(cachePayload)).toBe(true);
    await cache.getByRole("button", { name: "Clean cache", exact: true }).click();
    await expect(cache.getByRole("status")).toContainText("Removed");
    expect(existsSync(cachePayload)).toBe(false);
    expect(readFileSync(retained, "utf8")).toBe("keep");
    await page.screenshot({ path: test.info().outputPath("cache-cleanup.png") });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="large.safetensors"]').click();
    await expect(page.getByRole("cell", { name: "weights", exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "large.safetensors"))).toEqual(pointers.get("large.safetensors"));
    await page.locator('[role="treeitem"][data-item-path="staged.safetensors"]').click();
    await expect(page.getByRole("cell", { name: "weights", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "File materialized on disk" })).toBeVisible();
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(stagedContent);
    expect(git(["show", ":staged.safetensors"])).toEqual(pointers.get("staged.safetensors"));
    await page.getByRole("button", { name: "Back to preview", exact: true }).click();
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await expect(inspector.getByText("Working file", { exact: true })).toBeVisible();
    await expect(page.getByText("1 hydrated", { exact: true })).toBeVisible();
    await expect(page.getByText("/ 3 hydrated", { exact: false })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("materialized-staged-file.png") });
    await call(page, "close", { repo_id: "large-files" });
    await page.getByRole("button", { name: "Refresh hydration status", exact: true }).click();
    await expect(page.getByText("File status unavailable", { exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("file-status-unavailable.png") });
    await call(page, "open_local", { repo_id: "large-files", path: source });
    await page.getByRole("button", { name: "Refresh hydration status", exact: true }).click();
    await expect(page.getByText("1 hydrated", { exact: true })).toBeVisible();
    await expect(page.getByText("File status unavailable", { exact: true })).toHaveCount(0);
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
