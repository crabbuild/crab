import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { createHash, randomUUID } from "node:crypto";

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

test("packaged desktop opens and reads a real repository through its v2 agent", async () => {
  const executablePath = packagedExecutable();
  const root = mkdtempSync(path.join(tmpdir(), "crab-desktop-smoke-"));
  const git = (args: string[]) => execFileSync("git", args, { cwd: root });
  git(["init", "--initial-branch=main"]);
  writeFileSync(path.join(root, "desktop-smoke.txt"), "Crab Desktop v2 real-sidecar smoke\n");
  git(["add", "desktop-smoke.txt"]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Desktop fixture"]);
  const application = await electron.launch({ executablePath, args: [`--user-data-dir=${path.join(root, "profile")}`] });
  try {
    expect(await application.evaluate(({ app }) => app.getVersion())).toBe("2.0.0");
    const page = await application.firstWindow();
    await expect(page.locator("#root > *").first()).toBeVisible();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(() => page.evaluate(async () => {
      const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<{ pong?: string }> } }).crab;
      try { return (await bridge.rpc("ping", {})).pong; } catch { return undefined; }
    })).toBe("2.0.0");
    const text = await page.evaluate(async root => {
      const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<Record<string, unknown>> } }).crab;
      const handle = await bridge.rpc("open_local", { repo_id: "smoke", path: root });
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: root } }));
      return bridge.rpc("read_text", { repo_id: "smoke", path: "desktop-smoke.txt" });
    }, root);
    expect(text.text).toBe("Crab Desktop v2 real-sidecar smoke\n");
    await expect(page.getByText(path.basename(root), { exact: true }).first()).toBeVisible();
    await page.getByRole("button", { name: "Explorer", exact: true }).click();
    await page.screenshot({ path: test.info().outputPath("repository-opened.png") });
    await page.locator('[role="treeitem"][data-item-path="desktop-smoke.txt"]').click();
    await expect(page.getByText("Crab Desktop v2 real-sidecar smoke", { exact: true }).first()).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("desktop-v2.png") });
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
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBe("2.0.0");
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
      await expect.poll(async () => { try { return (await call(restored, "ping", {})).pong; } catch { return undefined; } }).toBe("2.0.0");
      await call(restored, "open_local", { repo_id: "restored", path: clone });
      expect((await call(restored, "storage_connections_list", { repo_id: "restored" })).selected_id).toBe(storage_connection_id);
      expect((await call(restored, "storage_config_verify", { repo_id: "restored" })).ok).toBe(true);
    } finally { await restarted.close(); }

  } finally {
    await application.close();
    rmSync(root, { recursive: true, force: true });
  }
});
