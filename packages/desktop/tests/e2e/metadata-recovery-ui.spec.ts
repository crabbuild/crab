import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, renameSync, readFileSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { AgentEvent } from "../../src/ipc";
import { isNativeAgentCommand, call, packagedExecutable } from "./packaged-app";

for (const { view, configured, parquet } of [{ view: "Dashboard", configured: false, parquet: false }, { view: "Datasets", configured: true, parquet: false }, { view: "Datasets", configured: false, parquet: false }, { view: "Datasets", configured: false, parquet: true }] as const) test(`${view} recovers metadata without losing its UI state after same-ID native repair (${configured ? "Crab configured" : "Git only"}, ${parquet ? "Parquet" : "Safetensors"})`, async () => {
  test.skip(process.platform === "win32", "The native process fixture uses POSIX process enumeration");
  test.setTimeout(120_000);
  const executablePath = packagedExecutable();
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-metadata-recovery-"));
  const source = path.join(root, "source"); mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", AWS_ACCESS_KEY_ID: "invalid-metadata-fixture", AWS_SECRET_ACCESS_KEY: "invalid-metadata-fixture", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  const weights = path.join(source, "weights.safetensors");
  const rulesFile = path.join(source, ".crab/desktop/dataset-rules.json");
  const rules = (kind: "training-data" | "eval-data") => writeFileSync(rulesFile, JSON.stringify({ rules: [{ kind, globs: ["**/*.parquet"] }] }));
  function tensorFile(count: number) {
    const size = 4 * 1024 ** 2;
    const tensors = Object.fromEntries(Array.from({ length: count }, (_, i) => [`tensor_${i}`, { dtype: "U8", shape: [size / count], data_offsets: [i * size / count, (i + 1) * size / count] }]));
    const header = Buffer.from(JSON.stringify(tensors)), prefix = Buffer.alloc(8);
    prefix.writeBigUInt64LE(BigInt(header.length));
    return Buffer.concat([prefix, header, Buffer.alloc(size, 0x42)]);
  }
  const config = path.join(source, ".git"), offline = path.join(source, ".git-offline");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  const events: Array<{ repo_id: string; native_session_id: string | null }> = [];
  try {
    git(["init", "--initial-branch=main"]);
    if (configured) {
      // Metadata inspection needs the declared project configuration, not a
      // provisioned cloud root. All content in this fixture is ordinary Git.
      writeFileSync(path.join(source, "crab.toml"), 'version = 1\n[remote]\nurl = "crab://fixture-bucket/metadata-recovery"\n');
      git(["remote", "add", "origin", "crab://fixture-bucket/metadata-recovery"]);
    }
    writeFileSync(path.join(source, "seed.txt"), "Initial line\n");
    writeFileSync(weights, tensorFile(1));
    // Tiny real Parquet inputs exercise native footer parsing and recovery
    // without requiring Python in the packaged smoke environment.
    const parquetBefore = path.join(__dirname, "../fixtures/metadata-before.parquet");
    const parquetAfter = path.join(__dirname, "../fixtures/metadata-after.parquet");
    if (parquet) {
      writeFileSync(path.join(source, "weights.parquet"), readFileSync(parquetBefore));
      mkdirSync(path.dirname(rulesFile), { recursive: true });
      writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
      rules("training-data");
    }
    git(["add", "."]);
    const commit = (message: string) => git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", message]);
    commit("Before native restart");
    application = await electron.launch({ executablePath, env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), args: [`--user-data-dir=${path.join(root, "profile")}`] });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-48_000); });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await page.exposeFunction("recordMetadataOwner", (value: typeof events[number]) => events.push(value));
    await page.evaluate(() => {
      const target = window as typeof window & { recordMetadataOwner: (value: unknown) => Promise<void> };
      window.crab!.onEvent((event: AgentEvent) => { if (event.method === "repo:capabilities-changed") void target.recordMetadataOwner(event.params); });
    });
    const handle = await call(page, "open_local", { repo_id: "metadata-recovery", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    const navigation = page.getByRole("navigation", { name: "Primary", exact: true });
    const cacheKey = "crab.dashboard.cache.metadata-recovery.codeFrequency_52";
    let cached: string | null = null;
    if (view === "Dashboard") {
      await expect(page.getByText("Before native restart", { exact: true }).and(page.locator(":visible")).first()).toBeVisible();
      await expect.poll(() => page.evaluate(key => localStorage.getItem(key), cacheKey)).not.toBeNull();
      cached = await page.evaluate(key => localStorage.getItem(key), cacheKey);
      await page.getByRole("button", { name: "Month", exact: true }).click();
    } else {
      await navigation.getByRole("button", { name: "Large Files", exact: true }).click();
      await page.getByRole("button", { name: "Datasets", exact: true }).click();
      const file = parquet ? /^weights\.parquet/ : /^weights\.safetensors/;
      await page.getByRole("button", { name: file }).click();
      const label = parquet ? "Columns" : "Tensors";
      await expect(page.getByText(label, { exact: true }).locator("..")).toHaveText(`${label}1`);
      await page.getByPlaceholder("Filter files…", { exact: true }).fill("weights");
    }
    renameSync(config, offline);
    const rows = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line));
    const native = rows.find(row => row && Number(row[2]) === application!.process().pid && isNativeAgentCommand(row[3]));
    if (!native) throw new Error("Test application's native child was not found");
    process.kill(Number(native[1]), "SIGKILL");
    await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
    renameSync(offline, config);
    const replacement = tensorFile(2);
    writeFileSync(weights, replacement);
    if (parquet) {
      writeFileSync(path.join(source, "weights.parquet"), readFileSync(parquetAfter));
      rules("eval-data");
    }
    writeFileSync(path.join(source, "weights-added.safetensors"), tensorFile(1));
    writeFileSync(path.join(source, "seed.txt"), "Initial line\nA new line\nAnother new line\n");
    git(["add", "."]); commit("After native restart");
    const index = readFileSync(path.join(source, ".git/index"));
    await page.getByRole("button", { name: "Retry reopening repository", exact: true }).click();
    await expect.poll(() => events.some(event => event.repo_id === handle.repo_id && typeof event.native_session_id === "string" && event.native_session_id !== handle.native_session_id)).toBe(true);
    if (view === "Dashboard") {
      await expect(page.getByText("After native restart", { exact: true }).and(page.locator(":visible")).first()).toBeVisible();
      await expect(page.getByText("Unable to load some repository data", { exact: true })).not.toBeVisible();
      await expect(page.getByRole("button", { name: "Month", exact: true })).toHaveClass(/shadow-sm/);
      await expect.poll(() => page.evaluate(key => localStorage.getItem(key), cacheKey)).not.toBe(cached);
    } else {
      await expect(page.getByRole("button", { name: /^weights-added\.safetensors/ })).toBeVisible();
      const label = parquet ? "Columns" : "Tensors";
      await expect(page.getByText(label, { exact: true }).locator("..")).toHaveText(`${label}2`);
      if (parquet) {
        await expect(page.getByText("Rows", { exact: true }).locator("..")).toHaveText("Rows4");
        await expect(page.getByRole("button", { name: /^Evaluation Data/ })).toBeVisible();
      }
      await expect(page.getByPlaceholder("Filter files…", { exact: true })).toHaveValue("weights");
    }
    expect(readFileSync(weights)).toEqual(replacement);
    if (parquet) expect(readFileSync(path.join(source, "weights.parquet"))).toEqual(readFileSync(parquetAfter));
    expect(readFileSync(path.join(source, ".git/index"))).toEqual(index);
    await expect(page.getByText("Agent crashed", { exact: true })).not.toBeVisible();
    await page.screenshot({ path: test.info().outputPath(`${view.toLowerCase()}-same-id-recovered.png`) });
    writeFileSync(test.info().outputPath("metadata-recovery-proof.json"), JSON.stringify({ view, parquet, configured, sameLogicalId: true, newNativeEpoch: true, dataRefreshed: true, uiStatePreserved: true, contentAndIndexPreserved: true, events }, null, 2));
  } finally {
    if (existsSync(offline)) renameSync(offline, config);
    if (application) {
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
