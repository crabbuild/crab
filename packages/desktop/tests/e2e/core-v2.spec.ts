import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, realpathSync, rmSync, openSync, ftruncateSync, closeSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { createHash, randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { parse } from "yaml";

const desktopVersion: string = JSON.parse(readFileSync(path.resolve(__dirname, "../../package.json"), "utf8")).version;
const [coreVersion, prerelease] = desktopVersion.split("-");
const [major, minor, patch] = coreVersion.split(".").map(Number);
const nextVersion = prerelease
  ? `${coreVersion}-${prerelease.split(".")[0]}.${Number(prerelease.split(".")[1]) + 1}`
  : `${major}.${minor}.${patch + 1}`;

function packagedExecutable(): string {
  const release = path.resolve(__dirname, "../../release");
  const candidates = process.platform === "darwin"
    ? ["mac-arm64/Crab.app/Contents/MacOS/Crab", "mac/Crab.app/Contents/MacOS/Crab"]
    : process.platform === "win32" ? ["win-unpacked/Crab.exe"] : ["linux-unpacked/crab"];
  const executablePath = process.env.CRAB_PACKAGED_APP ?? candidates.map(file => path.join(release, file)).find(existsSync);
  if (!executablePath) throw new Error("Build the unpacked application with npm run package:dir first");
  return executablePath;
}

async function call(page: Page, method: string, params: unknown): Promise<Record<string, unknown>> {
  return page.evaluate(async ({ method, params }) => {
    const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<Record<string, unknown>> } }).crab;
    return bridge.rpc(method, params);
  }, { method, params });
}

test("packaged desktop previews a 3 GiB Crab file and LFS pointers without hydration", async () => {
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
    writeFileSync(path.join(source, ".gitattributes"), "large.safetensors filter=crab -text\nlfs.safetensors filter=lfs -text\n");
    const sizes = { "large.safetensors": 3 * 1024 ** 3, "lfs.safetensors": 4 * 1024 ** 2 };
    for (const [file, size] of Object.entries(sizes)) {
      const header = Buffer.from(JSON.stringify({ weights: { dtype: "U8", shape: [size], data_offsets: [0, size] } }));
      const prefix = Buffer.alloc(8);
      prefix.writeBigUInt64LE(BigInt(header.length));
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
      const range = await call(page, "read_range", { repo_id: "large-files", path: file, offset: size - 16, length: 16 });
      expect(Buffer.from(range.bytes_b64 as string, "base64")).toEqual(Buffer.alloc(16));
      expect(readFileSync(path.join(source, file))).toEqual(pointers.get(file));
      await page.screenshot({ path: test.info().outputPath(`${file}.png`) });
    }
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
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("packaged updater handles CLI-only releases and genuine desktop updates", async () => {
  test.skip(process.platform === "linux", "The unpacked Linux build is not an installed AppImage or DEB");
  test.setTimeout(90_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const updateConfig = parse(readFileSync(path.join(resources, "app-update.yml"), "utf8"));
  expect(updateConfig).toMatchObject({ provider: "github", owner: "crabbuild", repo: "crab", channel: prerelease ? prerelease.split(".")[0] : "latest" });
  expect(updateConfig.updaterCacheDirName).toBeTruthy();
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-updater-"));
  const requests: string[] = [];
  let metadata: "missing" | "published" | "invalid" = "missing";
  const server = createServer((request, response) => {
    const url = new URL(request.url ?? "/", "http://localhost");
    requests.push(url.pathname);
    response.setHeader("Cache-Control", "no-store");
    if (url.pathname === "/crabbuild/crab/releases.atom") {
      response.setHeader("Content-Type", "application/atom+xml");
      response.end(`<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Crab v${nextVersion}</title><link href="https://github.com/crabbuild/crab/releases/tag/v${nextVersion}"/><content>Desktop release</content></entry></feed>`);
    } else if (url.pathname === "/crabbuild/crab/releases/latest") {
      response.setHeader("Content-Type", "application/json");
      response.end(JSON.stringify({ tag_name: `v${nextVersion}` }));
    } else if (url.pathname.startsWith(`/crabbuild/crab/releases/download/v${nextVersion}/`) && url.pathname.endsWith(".yml")) {
      if (metadata === "missing") { response.writeHead(404); response.end("No desktop metadata"); return; }
      response.setHeader("Content-Type", "text/yaml");
      response.end(metadata === "invalid" ? "version: [invalid" : `version: ${nextVersion}\nfiles:\n  - url: Crab-${nextVersion}.zip\n    sha512: ${Buffer.alloc(64).toString("base64")}\n    size: 1\n`);
    } else { response.writeHead(404); response.end("Unexpected release request"); }
  });
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Fixture server has no port");
  const base = `http://127.0.0.1:${address.port}`;
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`] });
    // Exercise electron-updater's real GitHub provider/HTTP parser through its
    // dedicated Electron session, without depending on mutable public releases.
    await application.evaluate(({ session }, base) => {
      session.fromPartition("electron-updater", { cache: false }).webRequest.onBeforeRequest(
        { urls: ["https://github.com/crabbuild/*"] },
        (details, callback) => { const url = new URL(details.url); callback({ redirectURL: base + url.pathname + url.search }); },
      );
    }, base);
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const check = () => page.evaluate(() => window.crab.updater.checkForUpdate());
    await expect(page.getByRole("status").filter({ hasText: "Automatic desktop updates are not published for this platform yet." })).toBeVisible({ timeout: 30_000 });
    expect((await check()).success).toBe(false);
    await expect(page.getByRole("link", { name: "View Crab releases" })).toHaveAttribute("href", "https://github.com/crabbuild/crab/releases");
    await expect(page.getByText("Update check failed:", { exact: false })).toHaveCount(0);
    expect(requests.every(url => url.startsWith("/crabbuild/crab/"))).toBe(true);
    await page.screenshot({ path: test.info().outputPath("desktop-updater.png") });
    metadata = "published";
    expect((await check()).version).toBe(nextVersion);
    await expect(page.getByRole("button", { name: "Download", exact: true })).toBeVisible();
    expect(requests.some(url => url.endsWith(".zip"))).toBe(false);
    metadata = "invalid";
    expect((await check()).success).toBe(false);
    await expect(page.getByRole("alert")).toHaveText("Could not update Crab. Check your connection and try again.");
  } finally {
    await application?.close();
    server.closeAllConnections();
    await new Promise<void>(resolve => server.close(() => resolve()));
    rmSync(root, { recursive: true, force: true });
  }
});

test("workflow commands stream output and surface failures through the packaged runner", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-workflow-output-"));
  const repo = path.join(root, "repo");
  const home = path.join(root, "home");
  mkdirSync(repo);
  mkdirSync(home);
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo });
  const workflow = `stages:
  example:
    cmd: "echo helloxxxx"
    outs:
      - example.out
  xxxx:
    cmd: "echo hhhh"
    deps:
      - stage_out:
          stage: "example"
          out: "example.out"
`;
  writeFileSync(path.join(repo, "crab.yaml"), workflow);
  execFileSync("git", ["add", "crab.yaml"], { cwd: repo });
  execFileSync("git", ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Workflow fixture"], { cwd: repo });
  const environment = { ...process.env };
  delete environment.CRAB_WORKFLOW_ENABLED;
  const application = await electron.launch({
    executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`],
    env: { ...environment, HOME: home, USERPROFILE: home, CRAB_CACHE_DIR: path.join(root, "cache") },
  });
  try {
    const page = await application.firstWindow();
    const rendererErrors: string[] = [];
    page.on("pageerror", error => rendererErrors.push(error.message));
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => {
      try { return (await call(page, "ping", {})).pong; } catch { return undefined; }
    }).toBe(desktopVersion);
    await page.evaluate(async repo => {
      const handle = await window.crab.rpc("open_local", { repo_id: "workflow-output", path: repo });
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: repo } }));
    }, repo);
    const primary = page.getByRole("navigation", { name: "Primary", exact: true });
    await primary.getByRole("button", { name: "Data Workflows", exact: true }).click();
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Completed", { exact: true })).toBeVisible();
    const output = page.getByRole("region", { name: "Workflow output", exact: true });
    await expect(output.getByText("helloxxxx", { exact: true })).toBeVisible();
    await expect(output.getByText("hhhh", { exact: true })).toBeVisible();
    await expect(output.getByText("example: Completed", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(repo, "example.out"), "utf8")).toBe("helloxxxx\n");
    await page.screenshot({ path: test.info().outputPath("workflow-output-success.png") });

    await primary.getByRole("button", { name: "Files", exact: true }).click();
    const failedCommand = { argv: [process.execPath, "-e", "console.error('stage-failure'); process.exit(7)"] };
    writeFileSync(path.join(repo, "crab.yaml"), workflow.replace('cmd: "echo hhhh"', `cmd: ${JSON.stringify(failedCommand)}`));
    await primary.getByRole("button", { name: "Data Workflows", exact: true }).click();
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(page.getByText("Failed", { exact: true })).toBeVisible();
    await expect(output.getByText("stage-failure", { exact: true })).toBeVisible();
    await expect(output.getByRole("alert")).toBeVisible();
    await expect(output.getByText(/xxxx: Failed/)).toBeVisible();
    await expect(output.getByText("helloxxxx", { exact: true })).toHaveCount(0);
    await expect(output.getByText("Run a workflow to see output here.", { exact: true })).toHaveCount(0);
    await page.screenshot({ path: test.info().outputPath("workflow-output-failure.png") });

    mkdirSync(path.join(repo, ".crab"), { recursive: true });
    writeFileSync(path.join(repo, ".crab", "local.toml"), "[workflow]\nenabled = false\n");
    await page.getByRole("button", { name: "Run All", exact: true }).click();
    await expect(output.getByRole("alert")).toContainText("workflow feature is disabled");
    await expect(output.getByText("stage-failure", { exact: true })).toHaveCount(0);
    expect(rendererErrors).toEqual([]);
  } finally {
    await application.close();
    // Agent discovery probes can finish writing after Electron closes.
    rmSync(root, { recursive: true, force: true, maxRetries: 5 });
  }
});

test("packaged desktop opens and reads a real repository through its v2 agent", async () => {
  test.setTimeout(60_000);
  const executablePath = packagedExecutable();
  const root = mkdtempSync(path.join(tmpdir(), "crab-desktop-smoke-"));
  const cache = path.join(root, "cache");
  mkdirSync(cache);
  const git = (args: string[]) => execFileSync("git", args, { cwd: root });
  git(["init", "--initial-branch=main"]);
  writeFileSync(path.join(root, "desktop-smoke.txt"), "Crab Desktop v2 real-sidecar smoke\n");
  git(["add", "desktop-smoke.txt"]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Desktop fixture"]);
  git(["branch", "topic"]);
  const application = await electron.launch({
    executablePath, args: [`--user-data-dir=${path.join(root, "profile")}`],
    env: { ...process.env, CRAB_PTY_MODE: "auto", CRAB_CACHE_DIR: cache },
  });
  try {
    expect(await application.evaluate(({ app }) => app.getVersion())).toBe(desktopVersion);
    const page = await application.firstWindow();
    page.on("pageerror", error => console.error("Renderer failure:", error.stack));
    page.on("console", message => { if (message.type() === "error") console.error("Renderer console:", message.text()); });
    await expect(page.locator("#root > *").first()).toBeVisible();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(() => page.evaluate(async () => {
      const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<{ pong?: string }> } }).crab;
      try { return (await bridge.rpc("ping", {})).pong; } catch { return undefined; }
    })).toBe(desktopVersion);
    expect(await page.evaluate(async () => (await window.crab.getPtyMode()).resolvedMode)).toBe("daemon");
    const text = await page.evaluate(async root => {
      const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<Record<string, unknown>> } }).crab;
      const handle = await bridge.rpc("open_local", { repo_id: "smoke", path: root });
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: root } }));
      return bridge.rpc("read_text", { repo_id: "smoke", path: "desktop-smoke.txt" });
    }, root);
    expect(text.text).toBe("Crab Desktop v2 real-sidecar smoke\n");
    const activity = await call(page, "git_activity", { repo_id: "smoke" });
    expect(activity.days).toEqual([expect.objectContaining({ count: 1 })]);
    await expect(page.getByText(path.basename(root), { exact: true }).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "Kanban Board", exact: true })).toHaveCount(0);
    expect(await page.evaluate(() => Object.hasOwn(window.crab, "kanban"))).toBe(false);
    await expect(page.getByRole("button", { name: "Chat", exact: true })).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Skills", exact: true })).toHaveCount(0);
    const agentDetection = await page.evaluate(() => window.crab.orchestrator.detectAgents());
    expect(agentDetection.success).toBe(true);
    await page.getByRole("button", { name: "Chat", exact: true }).click();
    if (!agentDetection.agents.some((agent: { available: boolean }) => agent.available)) {
      await page.getByRole("button", { name: "Skip for now", exact: true }).click();
    }
    const composer = page.getByPlaceholder("Ask anything. / for commands, @ for context, # for files…");
    const draft = "Keep this draft while managing skills @";
    await page.setViewportSize({ width: 880, height: 560 });
    await composer.fill(draft);
    const chatNavigation = page.getByRole("navigation", { name: "Agent chat", exact: true });
    await chatNavigation.getByRole("button", { name: "Skills", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Skills", exact: true })).toBeVisible();
    const skillSearch = page.getByPlaceholder("Search skills...");
    await expect(skillSearch).toBeVisible();
    await skillSearch.fill("Unmatched skill search");
    await skillSearch.press("Enter");
    await skillSearch.fill("");
    await expect(composer).toBeHidden();
    await page.getByRole("button", { name: "New Skill", exact: true }).click();
    await expect(page.getByRole("heading", { name: "New Skill", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await page.getByRole("button", { name: "Install from URL", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Install Skill from URL", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await page.screenshot({ path: test.info().outputPath("skills-in-chat.png") });
    await chatNavigation.getByRole("button", { name: "Chats", exact: true }).click();
    await expect(composer).toHaveValue(draft);
    await page.getByRole("button", { name: "Add context", exact: true }).click();
    await page.getByRole("menuitem", { name: /^Instructions/ }).click();
    const contextDialog = page.getByRole("dialog", { name: "Add context", exact: true });
    await expect(contextDialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(contextDialog).toBeHidden();
    await page.setViewportSize({ width: 1280, height: 820 });
    await expect(page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Health & Diagnostics", exact: true })).toHaveCount(0);
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("button", { name: "Diagnostics", exact: true }).click();
    const healthReport = page.getByRole("region", { name: "Health & Diagnostics", exact: true });
    await expect(healthReport.getByRole("heading", { name: "Health & Diagnostics", exact: true })).toBeVisible();
    await expect(healthReport.getByText("Agent Version", { exact: true })).toBeVisible();
    await expect(healthReport.getByText(desktopVersion, { exact: true }).first()).toBeVisible();
    await expect(healthReport.getByText(cache, { exact: true })).toBeVisible();
    const refreshHealth = healthReport.getByRole("button", { name: "Refresh health data", exact: true });
    await refreshHealth.click();
    await expect(refreshHealth).toBeEnabled();
    await expect(healthReport.getByRole("button", { name: "Copy diagnostics to clipboard", exact: true })).toBeVisible();
    await expect(page.getByRole("heading", { name: "PTY Daemon", exact: true, level: 3 })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("diagnostics-in-settings.png") });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.screenshot({ path: test.info().outputPath("repository-opened.png") });
    await page.locator('[role="treeitem"][data-item-path="desktop-smoke.txt"]').click();
    await expect(page.getByText("Crab Desktop v2 real-sidecar smoke", { exact: true }).first()).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("desktop-v2.png") });
    const primary = page.getByRole("navigation", { name: "Primary", exact: true });
    await expect(primary.getByRole("button", { name: "Data Workflows", exact: true })).toBeVisible();
    await expect(primary.getByRole("button", { name: "SSH Connections", exact: true })).toBeVisible();
    await expect(primary.getByRole("button", { name: "Terminal", exact: true })).toBeVisible();
    await expect(primary.getByRole("button", { name: "More tools", exact: true })).toHaveCount(0);
    await primary.getByRole("button", { name: "Git", exact: true }).click();
    const gitNavigation = page.getByRole("navigation", { name: "Git", exact: true });
    await gitNavigation.getByRole("button", { name: "History", exact: true }).click();
    await expect(gitNavigation.getByRole("button", { name: "History", exact: true })).toHaveAttribute("aria-pressed", "true");
    await expect(gitNavigation.getByRole("button")).toHaveText(["Changes", "History", "Branches", "Worktrees"]);
    await gitNavigation.getByRole("button", { name: "Branches", exact: true }).click();
    await expect(page.getByRole("button", { name: "Branches & Worktrees", exact: true })).toHaveCount(0);
    await expect(page.getByRole("tab", { name: "Branches", exact: true })).toHaveCount(0);
    await page.getByRole("option").filter({ hasText: "topic" }).click();
    await expect(page.getByRole("heading", { name: "topic", exact: true, level: 1 })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("git-branches.png") });
    await gitNavigation.getByRole("button", { name: "Worktrees", exact: true }).click();
    await expect(page.getByRole("heading", { name: "topic", exact: true, level: 1 })).toHaveCount(0);
    await expect(page.getByRole("tab", { name: "Worktrees", exact: true })).toHaveCount(0);
    await page.getByRole("button", { name: `View worktree ${path.basename(root)}`, exact: true }).click();
    await expect(page.getByTitle("Close detail panel", { exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("git-worktrees.png") });
    await page.setViewportSize({ width: 880, height: 560 });
    await expect(gitNavigation.getByRole("button", { name: "Branches", exact: true })).toBeVisible();
    await expect(gitNavigation.getByRole("button", { name: "Worktrees", exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("git-tabs-compact.png") });
    await page.setViewportSize({ width: 1280, height: 820 });
    await page.getByRole("button", { name: "Go back", exact: true }).click();
    await expect(gitNavigation.getByRole("button", { name: "Branches", exact: true })).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByRole("heading", { name: "topic", exact: true, level: 1 })).toBeVisible();
    await page.getByRole("button", { name: "Go forward", exact: true }).click();
    await expect(gitNavigation.getByRole("button", { name: "Worktrees", exact: true })).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTitle("Close detail panel", { exact: true })).toBeVisible();
    await expect(primary.getByRole("button", { name: "Git", exact: true })).toHaveAttribute("aria-pressed", "true");
    await page.reload();
    await expect(page.getByRole("navigation", { name: "Git", exact: true }).getByRole("button", { name: "Worktrees", exact: true })).toHaveAttribute("aria-pressed", "true");
    await primary.getByRole("button", { name: "Data Workflows", exact: true }).click();
    await expect(page.getByRole("button", { name: "Create Workflow", exact: true })).toBeVisible();
    await primary.getByRole("button", { name: "SSH Connections", exact: true }).click();
    await expect(page.getByRole("heading", { name: "SSH Remote Connections", exact: true })).toBeVisible();
    const coldCommandProof = path.join(root, "terminal-cold-command-proof.txt");
    const writeShellPid = (file: string) => `node -e "require('fs').writeFileSync(process.argv[1],String(process.ppid))" "${file}"`;
    await page.evaluate(({ command, root }) => {
      window.dispatchEvent(new CustomEvent("crab:spawn-agent-terminal", { detail: { name: "Cold smoke command", command, cwd: root } }));
    }, { command: writeShellPid(coldCommandProof), root });
    await expect.poll(() => existsSync(coldCommandProof)).toBe(true);
    await primary.getByRole("button", { name: "SSH Connections", exact: true }).click();
    await expect(page.getByRole("heading", { name: "SSH Remote Connections", exact: true })).toBeVisible();
    await primary.getByRole("button", { name: "Terminal", exact: true }).click();
    const terminalPage = page.getByRole("region", { name: "Terminal page", exact: true });
    await expect(terminalPage).toBeVisible();
    await expect(primary.getByRole("button", { name: "Terminal", exact: true })).toHaveAttribute("aria-pressed", "true");
    const mainBounds = await page.getByRole("main").boundingBox();
    const terminalBounds = await terminalPage.boundingBox();
    expect(Math.abs(terminalBounds!.height - mainBounds!.height)).toBeLessThan(2);
    expect(Math.abs(terminalBounds!.y - mainBounds!.y)).toBeLessThan(2);
    await expect(page.locator('[data-panel-group-direction="vertical"]')).toHaveCount(0);
    await expect(terminalPage.locator(".xterm-screen:visible").first()).toBeVisible();
    const terminal = terminalPage.locator(".xterm-helper-textarea:visible").first();
    await terminal.focus();
    const proof = path.join(root, "terminal-proof.txt");
    await page.keyboard.type(writeShellPid(proof));
    await page.keyboard.press("Enter");
    await expect.poll(() => existsSync(proof) && readFileSync(proof, "utf8").trim().length > 0).toBe(true);
    const shellPid = readFileSync(proof, "utf8").trim();
    await primary.getByRole("button", { name: "Git", exact: true }).click();
    await expect(terminalPage).toBeHidden();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+t" : "Control+t");
    await page.getByRole("button", { name: "Go back", exact: true }).click();
    await expect(terminalPage).toBeVisible();
    await page.keyboard.press("Control+Backquote");
    await expect(terminalPage).toBeHidden();
    await page.getByRole("button", { name: "Open Terminal", exact: true }).click();
    await expect(terminalPage).toBeVisible();
    const reopenedProof = path.join(root, "terminal-reopened-proof.txt");
    await terminal.focus();
    await page.keyboard.type(writeShellPid(reopenedProof));
    await page.keyboard.press("Enter");
    await expect.poll(() => existsSync(reopenedProof) ? readFileSync(reopenedProof, "utf8").trim() : "").toBe(shellPid);
    await expect(terminalPage.locator(".xterm-screen:visible").first()).toBeVisible();
    const commandProof = path.join(root, "terminal-command-proof.txt");
    await primary.getByRole("button", { name: "Files", exact: true }).click();
    await page.evaluate(({ command, root }) => {
      window.dispatchEvent(new CustomEvent("crab:spawn-agent-terminal", { detail: { name: "Smoke command", command, cwd: root } }));
    }, { command: writeShellPid(commandProof), root });
    await expect(terminalPage).toBeVisible();
    await expect.poll(() => existsSync(commandProof)).toBe(true);
    await expect(primary.getByRole("button", { name: "Terminal", exact: true })).toHaveAttribute("aria-pressed", "true");
    await page.setViewportSize({ width: 880, height: 560 });
    await page.screenshot({ path: test.info().outputPath("navigation-compact.png") });
    await page.reload();
    await expect(page.getByRole("region", { name: "Terminal page", exact: true })).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Terminal", exact: true })).toHaveAttribute("aria-pressed", "true");

  } finally {
    await application.close();
    rmSync(root, { recursive: true, force: true });
  }
});


test("terminal restore is isolated by profile and repository", async () => {
  test.setTimeout(60_000);
  const root = mkdtempSync(path.join(tmpdir(), "crab-terminal-isolation-"));
  const testHome = path.join(root, "home");
  const repository = path.join(root, "repository");
  mkdirSync(testHome);
  mkdirSync(repository);
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repository });
  const profile = path.join(root, "profile-a");
  const env = { ...process.env, HOME: testHome, CRAB_CACHE_DIR: path.join(root, "cache") };
  let application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env });
  const savedLayout = { version: 1, tabs: [{ id: "old-tab", title: "Other repository shell", shell: "", cwd: path.join(root, "removed-repository") }], splits: [], activeTabIndex: 0 };
  try {
    let page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    expect(await page.evaluate(layout => window.crab.termSaveState({ persistenceKey: "other-repository", layout, scrollbackData: { "old-tab": "saved scrollback" } }), savedLayout)).toEqual({ success: true });
    expect(await page.evaluate(() => window.crab.termRestoreState("other-repository"))).toEqual({ layout: savedLayout, scrollbackData: { "old-tab": "saved scrollback" } });
    await page.evaluate(() => window.crab.termClearSession("old-tab", "other-repository"));
    expect((await page.evaluate(() => window.crab.termRestoreState("other-repository"))).scrollbackData).toEqual({});
    expect((await page.evaluate(repository => window.crab.termRestoreState(repository), repository)).layout).toBeNull();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBe(desktopVersion);
    const handle = await call(page, "open_local", { repo_id: "terminal-isolation", path: repository });
    await page.evaluate(({ handle, repository }) => {
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: repository } }));
    }, { handle, repository });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Terminal", exact: true }).click();
    const terminal = page.getByRole("region", { name: "Terminal page", exact: true });
    await expect(terminal).toBeVisible();
    await expect(terminal.getByText("Other repository shell", { exact: true })).toHaveCount(0);
    await terminal.locator(".xterm-helper-textarea:visible").first().focus();
    const proof = path.join(root, "working-directory.txt");
    await page.keyboard.type(`node -e "require('fs').writeFileSync(process.argv[1],process.cwd())" "${proof}"`);
    await page.keyboard.press("Enter");
    await expect.poll(() => existsSync(proof) ? readFileSync(proof, "utf8") : "").toBe(realpathSync(repository));
    await page.screenshot({ path: test.info().outputPath("terminal-isolation.png") });
    await application.close();
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile-b")}`], env });
    page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    expect((await page.evaluate(() => window.crab.termRestoreState("other-repository"))).layout).toBeNull();
    expect(existsSync(path.join(testHome, ".crab-desktop", "terminal-state.json"))).toBe(false);
  } finally {
    await application.close();
    // Agent version probes can finish writing their isolated HOME after Electron exits.
    rmSync(root, { recursive: true, force: true, maxRetries: 5 });
  }
});

test("workspace sidebar filters, pins, remembers recents and restores collapse and scroll", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(tmpdir(), "crab-workspace-navigation-"));
  const workspace = path.join(root, "workspace");
  const seed = path.join(root, "seed");
  const profile = path.join(root, "profile");
  mkdirSync(workspace);
  mkdirSync(seed);
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: seed });
  writeFileSync(path.join(seed, "file.txt"), "Workspace navigation fixture\n");
  execFileSync("git", ["add", "file.txt"], { cwd: seed });
  execFileSync("git", ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Workspace fixture"], { cwd: seed });
  for (let index = 1; index <= 30; index++) {
    execFileSync("git", ["clone", "--quiet", "--shared", seed, path.join(workspace, `repo-${String(index).padStart(2, "0")}`)]);
  }
  writeFileSync(path.join(workspace, "repo-25", "file.txt"), "Changed workspace fixture\n");
  const launch = () => electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env: { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache") } });
  let application = await launch();
  try {
    let page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const toolbar = page.locator("aside").first();
    const toolbarActions = toolbar.getByRole("button");
    await expect(toolbarActions.nth(0)).toHaveAttribute("title", "Open Repository");
    await expect(toolbarActions.nth(1)).toHaveAttribute("title", "Open Workspace");
    await expect(toolbarActions.nth(2)).toHaveAttribute("title", "Clone Repository");
    await page.evaluate(workspace => window.crab.workspace.open(workspace), workspace);
    const region = () => page.getByRole("region", { name: "workspace workspace", exact: true });
    await expect(region().getByRole("button", { name: "Pin repo-30", exact: true })).toBeAttached();
    const filter = page.getByRole("textbox", { name: "Filter repositories", exact: true });
    await filter.fill("repo-25");
    await expect(region().getByRole("button", { name: "Pin repo-01", exact: true })).toHaveCount(0);
    await region().getByRole("button", { name: "Pin repo-25", exact: true }).click();
    await expect(region().getByRole("list", { name: "workspace Pinned", exact: true })).toContainText("repo-25");
    await region().getByRole("button").filter({ hasText: /^repo-25/ }).click();
    await expect(page.getByRole("button", { name: "Repository overview", exact: true })).toContainText("repo-25");
    await expect(region().getByLabel("1 changed file", { exact: true })).toBeVisible({ timeout: 15_000 });
    await filter.fill("repo-26");
    await region().getByRole("button").filter({ hasText: /^repo-26/ }).click();
    await expect(region().getByRole("list", { name: "workspace Recent", exact: true })).toContainText("repo-26");
    await filter.fill("does-not-exist");
    await expect(region().getByText("No matching repositories", { exact: true })).toBeVisible();
    await filter.press("Escape");
    await expect(filter).toHaveValue("");
    await region().getByRole("button", { name: "workspace workspace actions", exact: true }).click();
    await expect(page.getByRole("menuitem", { name: "Clone Repository to Workspace", exact: true })).toBeVisible();
    await page.getByRole("menuitem", { name: "Refresh Workspace", exact: true }).click();
    await expect(page.getByRole("button", { name: "Repository overview", exact: true })).toContainText("workspace");
    await expect(region().getByRole("list", { name: "workspace Pinned", exact: true })).toContainText("repo-25");
    await expect(region().getByRole("list", { name: "workspace Recent", exact: true })).toContainText("repo-26");
    await expect.poll(() => page.evaluate(() => Object.entries(localStorage).some(([key, value]) => key.startsWith("crab.workspaceSidebar.") && JSON.parse(value).recent["repo-26"] > 0))).toBe(true);
    await page.screenshot({ path: test.info().outputPath("workspace-sidebar-groups.png") });
    await region().getByRole("button", { name: "Collapse workspace", exact: true }).click();
    await expect(region().getByRole("button", { name: "Expand workspace", exact: true })).toBeVisible();
    await expect.poll(() => page.evaluate(() => Object.entries(localStorage).some(([key, value]) => key.startsWith("crab.workspaceSidebar.") && JSON.parse(value).collapsed === true))).toBe(true);
    await application.close();
    application = await launch();
    page = await application.firstWindow();
    page.on("pageerror", error => console.error("Renderer failure:", error.stack));
    page.on("console", message => { if (message.type() === "error") console.error("Renderer console:", message.text()); });
    await expect(region().getByRole("button", { name: "Expand workspace", exact: true })).toBeVisible({ timeout: 15_000 });
    await region().getByRole("button", { name: "Expand workspace", exact: true }).click();
    await expect(region().getByRole("list", { name: "workspace Pinned", exact: true })).toContainText("repo-25");
    await expect(region().getByRole("list", { name: "workspace Recent", exact: true })).toContainText("repo-26");
    await page.setViewportSize({ width: 880, height: 560 });
    await region().getByRole("button", { name: "Pin repo-30", exact: true }).focus();
    const repositoryList = page.getByLabel("Repository list", { exact: true });
    await expect.poll(() => repositoryList.evaluate(element => element.scrollTop)).toBeGreaterThan(100);
    const position = await repositoryList.evaluate(element => element.scrollTop);
    await page.screenshot({ path: test.info().outputPath("workspace-sidebar-compact.png") });
    await application.close();
    application = await launch();
    page = await application.firstWindow();
    await page.setViewportSize({ width: 880, height: 560 });
    await expect(region().getByRole("button", { name: "Pin repo-30", exact: true })).toBeAttached({ timeout: 15_000 });
    await expect.poll(() => page.getByLabel("Repository list", { exact: true }).evaluate(element => element.scrollTop)).toBeGreaterThan(position - 50);
  } finally {
    await application.close();
    rmSync(root, { recursive: true, force: true });
  }
});


test("native v2 add, push, remote reconstruction, clone and hydration", async () => {
  test.skip(!process.env.CRAB_SDK_TEST_S3_BUCKET, "Requires the isolated S3-compatible qualification fixture");
  test.setTimeout(180_000);
  const executablePath = packagedExecutable();
  const extension = process.platform === "win32" ? ".exe" : "";
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", `crab${extension}`);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-cloud-"));
  const source = path.join(root, "source");
  const clone = path.join(root, "clone");
  mkdirSync(source);
  const env = { ...process.env, PATH: `${path.dirname(crab)}${path.delimiter}${process.env.PATH ?? ""}` };
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  const url = `crab://${process.env.CRAB_SDK_TEST_S3_BUCKET}/desktop/${randomUUID()}`;
  git(["init", "--initial-branch=main"]);
  git(["config", "user.name", "Desktop test"]);
  git(["config", "user.email", "test@example.invalid"]);
  git(["config", "commit.gpgsign", "false"]);
  execFileSync(crab, ["init", url], { cwd: source, env });
  execFileSync(crab, ["track", "*.bin"], { cwd: source, env });
  const bytes = Buffer.alloc(2 * 1024 * 1024);
  for (let index = 0; index < bytes.length; index++) bytes[index] = (index * 31 + (index >>> 9)) % 251;
  writeFileSync(path.join(source, "data.bin"), bytes);
  const digest = (data: Uint8Array) => createHash("sha256").update(data).digest("hex");
  // Deliberately wrong ambient credentials and placement prove the saved
  // connection is used by both SDK reads and CLI subprocesses.
  const appEnv = { ...env, AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1" };
  const application = await electron.launch({ executablePath, env: appEnv, args: [`--user-data-dir=${path.join(root, "profile")}`] });
  try {
    const page = await application.firstWindow();
    await expect(page.locator("#root > *").first()).toBeVisible();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBe(desktopVersion);
    expect(await application.evaluate(() => process.env.AWS_ACCESS_KEY_ID === "invalid-ambient-access" && process.env.AWS_ENDPOINT_URL === "http://127.0.0.1:1")).toBe(true);
    const handle = await call(page, "open_local", { repo_id: "source", path: source });
    await page.evaluate(({ handle, source }) => {
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } }));
    }, { handle, source });
    await expect(page.getByText("source", { exact: true }).first()).toBeVisible();
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    const connections = page.getByRole("region", { name: "Storage connections" });
    await connections.getByRole("button", { name: "Add connection", exact: true }).click();
    await connections.getByLabel("Connection name", { exact: true }).fill("Qualification storage");
    await connections.getByLabel("Bucket", { exact: true }).fill(process.env.CRAB_SDK_TEST_S3_BUCKET!);
    await connections.getByLabel("Signing region", { exact: true }).fill(process.env.AWS_REGION!);
    await connections.getByLabel("Endpoint URL (optional)", { exact: true }).fill("http://127.0.0.1:1");
    await connections.getByLabel("Access key ID", { exact: true }).fill(process.env.AWS_ACCESS_KEY_ID!);
    await connections.getByLabel("Secret access key", { exact: true }).fill(process.env.AWS_SECRET_ACCESS_KEY!);
    await connections.getByRole("button", { name: "Test connection", exact: true }).click();
    await expect(connections.getByRole("alert")).toContainText("Cannot list this bucket", { timeout: 40_000 });
    await connections.getByLabel("Endpoint URL (optional)", { exact: true }).fill(process.env.AWS_ENDPOINT_URL!);
    await connections.getByRole("button", { name: "Test connection", exact: true }).click();
    await expect(connections.getByRole("status")).toContainText("Bucket read/list access verified", { timeout: 40_000 });
    await connections.getByRole("button", { name: "Save connection", exact: true }).click();
    await connections.getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(connections.getByRole("status")).toContainText("Connection selected for this repository");
    const saved = await call(page, "storage_connections_list", { repo_id: "source" });
    const storage_connection_id = saved.selected_id;
    expect(typeof storage_connection_id).toBe("string");
    expect(saved.connections).toEqual([expect.objectContaining({ name: "Qualification storage" })]);
    expect((saved.connections as object[])[0]).not.toHaveProperty("credentials");
    const encrypted = readFileSync(path.join(root, "profile", "storage-connections.enc"));
    expect(encrypted.includes(Buffer.from(process.env.AWS_SECRET_ACCESS_KEY!))).toBe(false);
    await page.screenshot({ path: test.info().outputPath("desktop-storage-connections.png") });
    const add = await call(page, "crab_add_structured", { repo_id: "source", job_id: randomUUID(), patterns: ["data.bin"] });
    expect((add.summary as { files_staged: number }).files_staged).toBe(1);
    await call(page, "git_add_paths", { repo_id: "source", paths: ["data.bin", ".gitattributes", "crab.toml"] });
    await call(page, "git_commit", { repo_id: "source", message: "Publish native v2 fixture", sign: false });
    const push = await call(page, "crab_push_structured", { repo_id: "source", job_id: randomUUID(), remote: "origin" });
    expect((push.summary as { refs_pushed: number }).refs_pushed).toBe(1);
    expect((await call(page, "git_fetch_all", { repo_id: "source" })).status).toBe(0);
    const storage = await call(page, "storage_summary", { repo_id: "source" });
    expect((storage.metadata as { protocol: string }).protocol).toBe("capsule-v2");
    await call(page, "open_url", { repo_id: "remote", url, storage_connection_id });
    const content = await call(page, "read_bytes", { repo_id: "remote", path: "data.bin", max_bytes: bytes.length + 1 });
    expect(digest(Buffer.from(content.bytes_b64 as string, "base64"))).toBe(digest(bytes));
    await call(page, "dehydrate", { job_id: randomUUID(), cwd: source, patterns: ["data.bin"] });
    expect(readFileSync(path.join(source, "data.bin")).length).toBeLessThan(1024);
    await call(page, "hydrate", { job_id: randomUUID(), cwd: source, patterns: ["data.bin"] });
    expect(digest(readFileSync(path.join(source, "data.bin")))).toBe(digest(bytes));
    await call(page, "clone", { job_id: randomUUID(), url, dest: clone, storage_connection_id });
    await call(page, "hydrate", { job_id: randomUUID(), cwd: clone, patterns: ["data.bin"] });
    expect(digest(readFileSync(path.join(clone, "data.bin")))).toBe(digest(bytes));
    const refs = await call(page, "list_refs", { repo_id: "remote" });
    expect((refs.refs as { name: string }[]).map(ref => ref.name)).toContain("main");
    writeFileSync(path.join(source, "crab.yaml"), JSON.stringify({ stages: { report: {
      cmd: { argv: ["node", "-e", "require('node:fs').writeFileSync('report.txt', 'workflow v2')"] },
      outs: ["report.txt"],
    } } }));
    const workflow = await call(page, "workflow_get", { repo_id: "source", path: "crab.yaml" });
    await call(page, "workflow_save", { repo_id: "source", workflow });
    const run = await call(page, "workflow_run", { repo_id: "source", run_id: randomUUID(), path: "crab.yaml", stages: ["report"] });
    expect((run.summary as { succeeded: string[] }).succeeded).toContain("report");
    expect(readFileSync(path.join(source, "report.txt"), "utf8")).toBe("workflow v2");
    await call(page, "close", { repo_id: "remote" });
    await page.screenshot({ path: test.info().outputPath("desktop-v2-storage.png") });
    await application.close();
    const restarted = await electron.launch({ executablePath, env: appEnv, args: [`--user-data-dir=${path.join(root, "profile")}`] });
    try {
      const restored = await restarted.firstWindow();
      await expect.poll(async () => { try { return (await call(restored, "ping", {})).pong; } catch { return undefined; } }).toBe(desktopVersion);
      await call(restored, "open_local", { repo_id: "restored", path: clone });
      expect((await call(restored, "storage_connections_list", { repo_id: "restored" })).selected_id).toBe(storage_connection_id);
      expect((await call(restored, "storage_config_verify", { repo_id: "restored" })).ok).toBe(true);
    } finally { await restarted.close(); }

  } finally {
    await application.close();
    rmSync(root, { recursive: true, force: true });
  }
});
