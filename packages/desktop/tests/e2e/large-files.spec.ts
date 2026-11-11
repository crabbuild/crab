import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, openSync, ftruncateSync, closeSync, statSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";

const desktopVersion: string = JSON.parse(readFileSync(path.resolve(__dirname, "../../package.json"), "utf8")).version;

test("packaged desktop previews large pointers and materializes Crab and LFS content", async () => {
  test.setTimeout(240_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-large-files-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", GIT_OPTIONAL_LOCKS: "0" };
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
    application = await electron.launch({
      executablePath,
      env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")),
      args: [`--user-data-dir=${path.join(root, "profile")}`],
    });
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
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
    await expect(inspector.getByRole("alert")).toContainText(/native content read failed/i);
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(pointers.get("lfs.safetensors"));
    await page.screenshot({ path: test.info().outputPath("lfs-materialization-integrity-error.png") });
    await page.getByRole("button", { name: "Back to preview", exact: true }).click();
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
    await expect(page.getByRole("status").filter({ hasText: "materialized on disk" })).toBeVisible();
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
    await page.locator('[role="treeitem"][data-item-path="lfs.safetensors"]').click();
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await expect(inspector.getByText("Git LFS", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "materialized on disk" })).toBeVisible();
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(git(["show", ":lfs.safetensors"])).toEqual(pointers.get("lfs.safetensors"));
    git(["diff", "--exit-code", "--", "staged.safetensors", "lfs.safetensors"]);
    await expect(page.getByText("2 hydrated", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Toggle Activity (background jobs)", exact: true }).click();
    await expect(page.getByText("Verified and materialized", { exact: true })).toHaveCount(2);
    await expect(page.getByText("Verified and materialized", { exact: true }).first()).toBeInViewport();
    await page.screenshot({ path: test.info().outputPath("materialized-lfs-and-crab.png") });

    await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
    writeFileSync(path.join(source, "staged.safetensors"), pointers.get("staged.safetensors")!);
    rmSync(path.join(source, "lfs.safetensors"));
    const tinyPath = "data[1].lfs";
    const tinyContent = Buffer.alloc(128, 7);
    writeFileSync(path.join(source, ".gitattributes"), "*.lfs filter=lfs -text\n", { flag: "a" });
    writeFileSync(path.join(source, tinyPath), tinyContent);
    git(["--literal-pathspecs", "add", "--", ".gitattributes", tinyPath]);
    const tinyPointer = git(["show", `:${tinyPath}`]);
    expect(tinyPointer.length).toBe(tinyContent.length);
    writeFileSync(path.join(source, tinyPath), tinyPointer);
    if (process.platform !== "win32") {
      const oid = git(["rev-parse", ":lfs.safetensors"]).toString().trim();
      git(["update-index", "--cacheinfo", `100755,${oid},lfs.safetensors`]);
    }
    const index = readFileSync(path.join(source, ".git", "index"));
    const untouched = readFileSync(path.join(source, "large.safetensors"));
    await page.getByRole("button", { name: "Refresh hydration status", exact: true }).click();
    await expect.poll(async () => {
      const status = await call(page, "hydration_status", { repo_id: "large-files" });
      return (status.items as { path: string; state: string }[]).find(item => item.path === tinyPath)?.state;
    }).toBe("pointer");
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await page.getByRole("tab", { name: "Custom", exact: true }).click();
    for (const file of ["staged.safetensors", "lfs.safetensors", tinyPath]) {
      await page.getByRole("checkbox", { name: new RegExp(file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")) }).check();
    }
    await expect.poll(() => page.evaluate(() => window.crab.getCapabilities("large-files"))).toMatchObject({ canHydrateSelection: true });
    await expect(page.getByRole("button", { name: /^Hydrate 3 files/ })).toBeEnabled();
    await page.getByRole("button", { name: /^Hydrate 3 files/ }).click();
    try {
      await expect(page.getByRole("main").getByText("Hydrated staged.safetensors", { exact: true })).toBeVisible();
    } catch (error) {
      await page.screenshot({ path: test.info().outputPath("hydrate-failure.png") });
      writeFileSync(test.info().outputPath("hydrate-failure.txt"), await page.getByRole("main").innerText());
      throw error;
    }
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(stagedContent);
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(readFileSync(path.join(source, tinyPath))).toEqual(tinyContent);
    if (process.platform !== "win32") expect(statSync(path.join(source, "lfs.safetensors")).mode & 0o100).toBe(0o100);
    expect(readFileSync(path.join(source, "large.safetensors"))).toEqual(untouched);
    expect(readFileSync(path.join(source, ".git", "index"))).toEqual(index);
    await page.getByRole("button", { name: "Toggle Activity (background jobs)", exact: true }).click();
    await expect(page.getByText("Verified and materialized", { exact: true })).toHaveCount(3);
    await expect(page.getByText("3 of 3 files materialized", { exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("bulk-crab-lfs-hydration.png") });

    await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
    writeFileSync(path.join(source, tinyPath), tinyPointer);
    writeFileSync(path.join(source, "lfs.safetensors"), pointers.get("lfs.safetensors")!);
    writeFileSync(payload, corrupt);
    await page.getByRole("button", { name: "Refresh hydration status", exact: true }).click();
    await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
    await expect(page.getByRole("alert").filter({ hasText: "Hydrate lfs.safetensors" })).toBeVisible();
    expect(readFileSync(path.join(source, tinyPath))).toEqual(tinyContent);
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(pointers.get("lfs.safetensors"));
    expect(readFileSync(path.join(source, ".git", "index"))).toEqual(index);
    await page.getByRole("button", { name: "Toggle Activity (background jobs)", exact: true }).click();
    const partial = page.getByText("1 of 2 files materialized", { exact: true });
    await expect(partial.locator("..").getByText("Materialization failed", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
    writeFileSync(payload, original);
    await page.getByRole("button", { name: /^Hydrate 1 file/ }).click();
    await expect(page.getByRole("main").getByText("Hydrated lfs.safetensors", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(readFileSync(path.join(source, ".git", "index"))).toEqual(index);

    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Commit mixed pointer selection"]);
    writeFileSync(path.join(source, "unrelated.txt"), "unrelated staged edit\n");
    git(["add", "--", "unrelated.txt"]);
    const dehydrateIndex = readFileSync(path.join(source, ".git", "index"));
    const dehydrate = async (count: number) => {
      await page.getByRole("button", { name: new RegExp(`^Dehydrate ${count} file`) }).click();
      await page.getByRole("dialog", { name: "Dehydrate files?" }).getByRole("button", { name: "Dehydrate", exact: true }).click();
    };
    writeFileSync(path.join(source, tinyPath), Buffer.alloc(tinyContent.length, 99));
    await dehydrate(3);
    await expect(page.getByRole("alert").filter({ hasText: "Working file differs from committed content" })).toBeVisible();
    expect(readFileSync(path.join(source, tinyPath))).toEqual(Buffer.alloc(tinyContent.length, 99));
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(stagedContent);
    writeFileSync(path.join(source, tinyPath), tinyContent);
    const project = path.join(source, "crab.toml");
    writeFileSync(project, '[remote]\nurl = "crab://fixture/repo"\n[prefetch.profiles.always]\npaths = ["*.lfs"]\n');
    await dehydrate(3);
    await expect(page.getByRole("alert").filter({ hasText: "always prefetch profile" })).toBeVisible();
    expect(readFileSync(path.join(source, tinyPath))).toEqual(tinyContent);
    rmSync(project);
    writeFileSync(payload, corrupt);
    await dehydrate(3);
    await expect(page.getByRole("alert").filter({ hasText: "lfs.safetensors" })).toBeVisible();
    expect(readFileSync(path.join(source, tinyPath))).toEqual(tinyPointer);
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(stagedContent);
    await page.getByRole("button", { name: "Toggle Activity (background jobs)", exact: true }).click();
    const stopped = page.getByText("1 of 3 files dehydrated", { exact: true });
    await expect(stopped.locator("..").getByText("Dehydration failed", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
    writeFileSync(payload, original);
    await dehydrate(2);
    await expect(page.getByRole("main").getByText("Dehydrated staged.safetensors", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(pointers.get("staged.safetensors"));
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(pointers.get("lfs.safetensors"));
    if (process.platform !== "win32") expect(statSync(path.join(source, "lfs.safetensors")).mode & 0o100).toBe(0o100);
    expect(readFileSync(path.join(source, ".git", "index"))).toEqual(dehydrateIndex);
    expect(readFileSync(path.join(source, "large.safetensors"))).toEqual(untouched);
    await page.screenshot({ path: test.info().outputPath("bulk-crab-lfs-dehydration.png") });
    const primary = page.getByRole("navigation", { name: "Primary", exact: true });
    await primary.getByRole("button", { name: "Files", exact: true }).click();
    await page.locator(`[role="treeitem"][data-item-path="${tinyPath}"]`).click({ button: "right" });
    await page.getByRole("button", { name: "Hydrate File", exact: true }).click();
    await expect.poll(() => readFileSync(path.join(source, tinyPath)).equals(tinyContent)).toBe(true);
    await primary.getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).click();
    await page.getByRole("tab", { name: "Custom", exact: true }).click();
    for (const file of ["staged.safetensors", "lfs.safetensors"]) {
      await page.getByRole("checkbox", { name: new RegExp(file) }).check();
    }
    await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
    await expect(page.getByRole("main").getByText("Hydrated staged.safetensors", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(stagedContent);
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(readFileSync(path.join(source, tinyPath))).toEqual(tinyContent);
    expect(readFileSync(path.join(source, ".git", "index"))).toEqual(dehydrateIndex);
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
