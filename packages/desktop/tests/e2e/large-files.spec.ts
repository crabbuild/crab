import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, openSync, ftruncateSync, closeSync, statSync, chmodSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";

const desktopVersion: string = JSON.parse(readFileSync(path.resolve(__dirname, "../../package.json"), "utf8")).version;

test("tracking wizard commits literal selected files and preserves unrelated staging", async () => {
  const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
  test.skip(!descriptor, "Requires the owned real-store tracking fixture");
  test.setTimeout(120_000);
  const storage = (JSON.parse(readFileSync(descriptor!, "utf8")) as { profiles: Array<{ bucket: string; endpoint: string; region: string; credentials: { access_key_id: string; secret_access_key: string } }> }).profiles[0];
  const remoteUrl = `crab://${storage.bucket}/proof`;
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-tracking-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"),
    GIT_CONFIG_NOSYSTEM: "1",
    CRAB_CACHE_DIR: path.join(root, "cache"),
    AWS_ACCESS_KEY_ID: storage.credentials.access_key_id,
    AWS_SECRET_ACCESS_KEY: storage.credentials.secret_access_key,
    AWS_ENDPOINT_URL: storage.endpoint,
    AWS_ENDPOINT_URL_S3: storage.endpoint,
    AWS_REGION: storage.region,
    AWS_DEFAULT_REGION: storage.region,
    AWS_EC2_METADATA_DISABLED: "true",
    AWS_ALLOW_HTTP: "true",
    AWS_VIRTUAL_HOSTED_STYLE_REQUEST: "false",
  };
  delete env.AWS_SESSION_TOKEN;
  delete env.AWS_PROFILE;
  writeFileSync(path.join(root, "gitconfig"), "");
  const git = (args: string[]) => execFileSync("git", ["--literal-pathspecs", ...args], { cwd: source, env });
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    git(["remote", "add", "origin", remoteUrl]);
    git(["config", "user.name", "Desktop fixture"]);
    git(["config", "user.email", "desktop@example.invalid"]);
    const driver = `'${crab.replaceAll("\\", "/").replaceAll("'", "'\\''")}' filter-process`;
    for (const kind of ["crab", "lfs"]) {
      git(["config", `filter.${kind}.process`, driver]);
      git(["config", `filter.${kind}.required`, "true"]);
    }
    writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
    writeFileSync(path.join(source, "crab.toml"), `[remote]\nurl = ${JSON.stringify(remoteUrl)}\n`);
    writeFileSync(path.join(source, ".gitattributes"), "lfs.bin filter=lfs -text\n");
    writeFileSync(path.join(source, "other.txt"), "original\n");
    git(["add", "--", ".gitignore", ".gitattributes", "other.txt"]);
    git(["commit", "-m", "initial"]);
    writeFileSync(path.join(source, "other.txt"), "keep this staged\n");
    git(["add", "--", "other.txt"]);
    const unrelated = git(["ls-files", "--stage", "-z", "--", "other.txt"]);
    const content = Buffer.alloc(2 * 1024 * 1024, 53);
    const selected = ["data[1].bin", "lfs.bin", "weights"];
    for (const file of [...selected, "data1.bin"]) writeFileSync(path.join(source, file), content);
    application = await electron.launch({ executablePath, args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "tracking", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Track & Stage", exact: true }).click();
    for (const file of selected) await page.getByText(file, { exact: true }).locator("../..").getByRole("checkbox").check();
    await page.getByRole("button", { name: "Track 3 files", exact: true }).click();
    if (process.platform === "win32") rmSync(path.join(source, "data[1].bin"));
    else chmodSync(path.join(source, "data[1].bin"), 0);
    await page.getByRole("button", { name: "Track 3 files", exact: true }).click();
    await expect(page.getByRole("button", { name: "Retry tracking", exact: true })).toBeVisible({ timeout: 30_000 });
    await expect(page.getByText("2 tracked, 1 failed", { exact: true })).toBeVisible();
    expect(git(["show", ":lfs.bin"]).toString()).toContain("version https://git-lfs.github.com/spec/v1");
    if (process.platform === "win32") writeFileSync(path.join(source, "data[1].bin"), content);
    else chmodSync(path.join(source, "data[1].bin"), 0o644);
    await page.getByRole("button", { name: "Retry tracking", exact: true }).click();
    await expect(page.getByText("Ready to commit", { exact: true })).toBeVisible();
    expect(git(["check-attr", "filter", "--", "data1.bin"]).toString()).toContain("unspecified");
    expect(git(["check-attr", "filter", "--", "lfs.bin"]).toString()).toContain("lfs");
    expect(git(["show", ":lfs.bin"]).toString()).toContain("version https://git-lfs.github.com/spec/v1");
    const edited = Buffer.from(content);
    edited.write("edited after tracking", 100);
    writeFileSync(path.join(source, "data[1].bin"), edited);
    await page.getByLabel("Commit message for tracked files", { exact: true }).fill("Track selected large files");
    const beforeCommit = git(["rev-parse", "HEAD"]).toString();
    await page.getByRole("button", { name: "Commit", exact: true }).click();
    await expect.poll(() => git(["rev-parse", "HEAD"]).toString(), { timeout: 30_000 }).not.toBe(beforeCommit);
    await expect(page.getByText(/^Committed/)).toBeVisible();
    expect(git(["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "HEAD"]).toString().split("\0").filter(Boolean).sort()).toEqual([".gitattributes", ...selected].sort());
    expect(git(["ls-files", "--stage", "-z", "--", "other.txt"])).toEqual(unrelated);
    expect(git(["show", "HEAD:other.txt"]).toString()).toBe("original\n");
    expect(git(["ls-files", "--", "data1.bin"]).length).toBe(0);
    for (const file of selected) {
      const saved = await page.evaluate(async params => {
        try { return await window.crab.rpc("read_bytes", params) as { bytes_b64: string }; }
        catch (error) { throw new Error(JSON.stringify(error)); }
      }, { repo_id: "tracking", path: file, rev: "HEAD", max_bytes: content.length + 1 });
      expect(Buffer.from(String(saved.bytes_b64), "base64")).toEqual(file === "data[1].bin" ? edited : content);
      expect(readFileSync(path.join(source, file))).toEqual(file === "data[1].bin" ? edited : content);
    }
    await page.screenshot({ path: test.info().outputPath("selected-tracking-commit.png") });
    await page.getByRole("button", { name: "Skip push — I'll push later", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await page.getByRole("tab", { name: "Pattern", exact: true }).click();
    const rule = page.getByText("/data\\[1\\].bin", { exact: true }).locator("../..");
    await expect(rule).toContainText("1 file");
    await page.screenshot({ path: test.info().outputPath("selected-tracking-pattern.png") });
  } catch (error) {
    if (application) {
      const page = await application.firstWindow();
      await page.screenshot({ path: test.info().outputPath("selected-tracking-failure.png") });
      writeFileSync(test.info().outputPath("selected-tracking-failure.txt"), await page.getByRole("main").innerText());
    }
    throw error;
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});

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
    await page.getByRole("button", { name: "Activity", exact: true }).click();
    await expect(page.getByText("Verified and materialized", { exact: true })).toHaveCount(2);
    const received = await call(page, "activity_file_transfers", {});
    const hydrated = (received.jobs as Array<{ op: string; status: string; transfer?: { received_bytes?: number } }>).filter(job => job.op === "hydrate" && job.status === "done");
    expect(hydrated).toHaveLength(2);
    for (const job of hydrated) expect(job.transfer?.received_bytes).toBe(0);
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
    // Publication refreshes verified stat entries; pointer identities, modes and
    // unrelated staging remain unchanged. Raw index cache bytes are not stable.
    const index = git(["ls-files", "--stage", "-z"]);
    const untouched = readFileSync(path.join(source, "large.safetensors"));
    await page.getByRole("button", { name: "Refresh hydration status", exact: true }).click();
    await expect.poll(async () => {
      const status = await call(page, "hydration_status", { repo_id: "large-files" });
      return (status.items as { path: string; state: string }[]).find(item => item.path === tinyPath)?.state;
    }).toBe("pointer");
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await page.getByRole("radio", { name: "Custom", exact: true }).click();
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
    expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
    await page.getByRole("button", { name: "Activity", exact: true }).click();
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
    expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
    await page.getByRole("button", { name: "Activity", exact: true }).click();
    const partial = page.getByText("1 of 2 files materialized", { exact: true });
    await expect(partial.locator("..").getByText("Materialization failed", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
    writeFileSync(payload, original);
    await page.getByRole("button", { name: /^Hydrate 1 file/ }).click();
    await expect(page.getByRole("main").getByText("Hydrated lfs.safetensors", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(git(["ls-files", "--stage", "-z"])).toEqual(index);

    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Commit mixed pointer selection"]);
    writeFileSync(path.join(source, "unrelated.txt"), "unrelated staged edit\n");
    git(["add", "--", "unrelated.txt"]);
    const dehydrateIndex = git(["ls-files", "--stage", "-z"]);
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
    await page.getByRole("button", { name: "Activity", exact: true }).click();
    const stopped = page.getByText("1 of 3 files dehydrated", { exact: true });
    await expect(stopped.locator("..").getByText("Dehydration failed", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
    writeFileSync(payload, original);
    await dehydrate(2);
    await expect(page.getByRole("main").getByText("Dehydrated staged.safetensors", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(pointers.get("staged.safetensors"));
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(pointers.get("lfs.safetensors"));
    if (process.platform !== "win32") expect(statSync(path.join(source, "lfs.safetensors")).mode & 0o100).toBe(0o100);
    expect(git(["ls-files", "--stage", "-z"])).toEqual(dehydrateIndex);
    expect(readFileSync(path.join(source, "large.safetensors"))).toEqual(untouched);
    await page.screenshot({ path: test.info().outputPath("bulk-crab-lfs-dehydration.png") });
    const primary = page.getByRole("navigation", { name: "Primary", exact: true });
    await primary.getByRole("button", { name: "Files", exact: true }).click();
    await page.locator(`[role="treeitem"][data-item-path="${tinyPath}"]`).click({ button: "right" });
    await page.getByRole("button", { name: "Hydrate File", exact: true }).click();
    await expect.poll(() => readFileSync(path.join(source, tinyPath)).equals(tinyContent)).toBe(true);
    await primary.getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).click();
    await page.getByRole("radio", { name: "Custom", exact: true }).click();
    for (const file of ["staged.safetensors", "lfs.safetensors"]) {
      await page.getByRole("checkbox", { name: new RegExp(file) }).check();
    }
    await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
    await expect(page.getByRole("main").getByText("Hydrated staged.safetensors", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "staged.safetensors"))).toEqual(stagedContent);
    expect(readFileSync(path.join(source, "lfs.safetensors"))).toEqual(original);
    expect(readFileSync(path.join(source, tinyPath))).toEqual(tinyContent);
    expect(git(["ls-files", "--stage", "-z"])).toEqual(dehydrateIndex);
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
