import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { AgentEvent } from "../../src/ipc";
import { isNativeAgentCommand, call, packagedExecutable } from "./packaged-app";

function nativeChild(application: Awaited<ReturnType<typeof electron.launch>>): number {
  const parent = application.process().pid;
  const children = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n");
  const child = children.map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).find(row => row && Number(row[2]) === parent && isNativeAgentCommand(row[3]));
  if (!child) throw new Error("Test application's native child was not found");
  return Number(child[1]);
}

async function showRepository(page: Page, handle: Record<string, unknown>, source: string) {
  await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
}

test("packaged native recovery reopens repositories and replaces retained file and media captures", async () => {
  test.skip(process.platform === "win32", "The process fixture uses POSIX process enumeration");
  test.setTimeout(120_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent/crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-native-recovery-"));
  const source = path.join(root, "source"), sibling = path.join(root, "sibling");
  mkdirSync(source); mkdirSync(sibling);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[], cwd = source) => execFileSync("git", args, { cwd, env });
  const content = Buffer.alloc(4 * 1024 ** 2, 0x37);
  const pointers = new Map<string, Buffer>();
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  const events: Array<{ repo_id: string; native_session_id: string | null; capabilities: { canMaterialize: boolean } }> = [];
  try {
    for (const cwd of [source, sibling]) git(["init", "--initial-branch=main"], cwd);
    mkdirSync(path.join(source, ".crab"));
    writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks"))}\n`);
    const driver = `'${crab.replaceAll("'", "'\\''")}' filter-process`;
    for (const kind of ["crab", "lfs"]) {
      git(["config", `filter.${kind}.process`, driver]); git(["config", `filter.${kind}.required`, "true"]);
    }
    writeFileSync(path.join(source, ".gitattributes"), "chunked.bin filter=crab -text\nlarge-lfs.bin filter=lfs -text\n");
    writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
    for (const file of ["chunked.bin", "large-lfs.bin"]) {
      writeFileSync(path.join(source, file), content); git(["add", file]);
      const pointer = git(["show", `:${file}`]); expect(pointer.length).toBeLessThan(1024);
      pointers.set(file, pointer); writeFileSync(path.join(source, file), pointer);
    }
    writeFileSync(path.join(source, "audio.wav"), Buffer.alloc(128));
    git(["add", ".gitattributes", ".gitignore", "audio.wav"]);
    for (const cwd of [source, sibling]) git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "Native recovery fixture"], cwd);
    const electronEnv = Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string"));
    application = await electron.launch({ executablePath, env: electronEnv, args: [`--user-data-dir=${path.join(root, "profile")}`] });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await page.exposeFunction("recordNativeRuntime", (value: typeof events[number]) => events.push(value));
    await page.evaluate(() => {
      const target = window as typeof window & { recordNativeRuntime: (value: unknown) => Promise<void> };
      window.crab!.onEvent((event: AgentEvent) => { if (event.method === "repo:capabilities-changed") void target.recordNativeRuntime(event.params); });
    });
    const other = await call(page, "open_local", { repo_id: "sibling", path: sibling });
    await showRepository(page, other, sibling);
    const handle = await call(page, "open_local", { repo_id: "recover", path: source });
    await showRepository(page, handle, source);
    const session = handle.native_session_id;
    expect(typeof session).toBe("string");
    const stale = await call(page, "file_open", { repo_id: "recover", path: "chunked.bin", native_session_id: session });
    const media = await call(page, "media_open", { repo_id: "recover", path: "audio.wav", native_session_id: session });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="chunked.bin"]').click();
    await expect(page.getByRole("button", { name: "Hydrate to disk", exact: true })).toBeEnabled();
    process.kill(nativeChild(application), "SIGKILL");
    for (const repo of ["recover", "sibling"]) {
      await expect.poll(() => events.some(event => event.repo_id === repo && event.native_session_id === null && !event.capabilities.canMaterialize)).toBe(true);
      await expect.poll(() => events.some(event => event.repo_id === repo && event.native_session_id !== null && event.native_session_id !== session && event.capabilities.canMaterialize)).toBe(true);
    }
    const staleFailure = await page.evaluate(async params => {
      try { await window.crab!.rpc("file_materialize", params); return null; }
      catch (error) { return error as { code?: string; message?: string }; }
    }, { repo_id: "recover", file_id: stale.file_id, native_session_id: session, op_id: "stale-capture" });
    expect(staleFailure).toMatchObject({ code: "NATIVE_SESSION_LOST", message: "The file reader's native session ended. Reopen the file." });
    expect(await page.evaluate(async url => (await fetch(url)).status, String(media.url))).toBe(410);
    await expect(call(page, "list", { repo_id: "sibling", path: "" })).resolves.toBeDefined();
    for (const file of ["chunked.bin", "large-lfs.bin"]) {
      if (file !== "chunked.bin") {
        await page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
      }
      await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
      await expect(page.getByText(/materialized on disk/)).toBeVisible();
      expect(readFileSync(path.join(source, file))).toEqual(content);
      expect(git(["show", `:${file}`])).toEqual(pointers.get(file));
    }
    await page.screenshot({ path: test.info().outputPath("native-recovered-materialization.png") });
    writeFileSync(test.info().outputPath("native-recovery-proof.json"), JSON.stringify({ events, exactCrabAndLfsBytes: true, indexPointersPreserved: true, staleFileRejected: true, staleMediaRevoked: true, siblingReopened: true }, null, 2));
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
