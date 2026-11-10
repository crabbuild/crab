import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { AgentEvent } from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";

for (const target of ["text preview", "Markdown editor", "pending text", "pending Markdown", "Changes file", "text diff", "LFS diff", "notebook diff"] as const) test(`${target} recaptures its selection after same-ID native repair`, async () => {
  test.skip(process.platform === "win32", "Requires POSIX process enumeration");
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-selected-recovery-"));
  const source = path.join(root, "source"); mkdirSync(source);
  const metadata = path.join(source, ".git"), offline = path.join(source, ".git-offline");
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  const file = target === "LFS diff" ? "weights.bin" : target === "notebook diff" ? "analysis.ipynb" : target.includes("Markdown") ? "note.md" : "note.txt";
  const digest = (value: string) => createHash("sha256").update(value).digest("hex");
  const value = (marker: string) => target === "LFS diff" ? `version https://git-lfs.github.com/spec/v1\noid sha256:${digest(marker)}\nsize 4096\n`
    : target === "notebook diff" ? JSON.stringify({ nbformat: 4, nbformat_minor: 5, metadata: {}, cells: [{ id: "selected", cell_type: "code", execution_count: null, metadata: {}, outputs: [], source: [marker] }] }) : `${marker}\n`;
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  const pending = target.startsWith("pending");
  const editor = target === "text preview" || target === "Markdown editor" || pending;
  const events: Array<{ repo_id: string; native_session_id: string | null }> = [];
  try {
    git(["init", "--initial-branch=main"]);
    writeFileSync(path.join(source, file), value("COMMITTED_SOURCE")); git(["add", file]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Selected source fixture"]);
    writeFileSync(path.join(source, file), value("BEFORE_NATIVE_REPAIR"));
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    // Electron owns the beforeunload decision. Playwright's automatic dialog
    // dismissal races that native handler and obscures the actual test result.
    page.on("dialog", dialog => { if (dialog.type() !== "beforeunload") void dialog.dismiss(); });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await page.exposeFunction("recordSelectedOwner", (event: typeof events[number]) => events.push(event));
    await page.evaluate(() => {
      const receiver = window as typeof window & { recordSelectedOwner: (event: unknown) => Promise<void> };
      window.crab!.onEvent((event: AgentEvent) => { if (event.method === "repo:capabilities-changed") void receiver.recordSelectedOwner(event.params); });
    });
    const handle = await call(page, "open_local", { repo_id: "selected-source", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    const navigation = page.getByRole("navigation", { name: "Primary", exact: true });
    if (editor) {
      await navigation.getByRole("button", { name: "Files", exact: true }).click();
      await page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
    } else {
      await navigation.getByRole("button", { name: "Git", exact: true }).click();
      await page.getByRole("navigation", { name: "Git", exact: true }).getByRole("button", { name: "Changes", exact: true }).click();
      await page.getByRole("radio", { name: "List", exact: true }).click();
      if (target === "Changes file") {
        await page.getByRole("listbox", { name: "Changed files", exact: true }).getByRole("option").filter({ has: page.getByText(file, { exact: true }) }).hover();
        await page.getByRole("button", { name: `Open file ${file}`, exact: true }).click();
      }
      else {
        await page.getByRole("listbox", { name: "Changed files", exact: true }).getByRole("option").filter({ has: page.getByText(file, { exact: true }) }).dblclick();
        await page.getByRole("combobox", { name: "Compare file versions", exact: true }).selectOption("unstaged");
      }
    }
    const content = editor ? page.locator(".monaco-editor .view-lines") : target === "Changes file" ? page.locator("pre")
      : target === "text diff" ? page.locator("diffs-container") : target === "LFS diff" ? page.getByTestId("large-file-diff") : page.getByRole("region", { name: "After Source", exact: true });
    await expect(content).toContainText(target === "LFS diff" ? digest("BEFORE_NATIVE_REPAIR") : "BEFORE_NATIVE_REPAIR");
    if (editor && !pending) await content.click();
    if (pending) {
      await application.evaluate(async (_electron, file) => {
        const fs = process.getBuiltinModule("fs").promises, original = fs.writeFile, open = fs.open;
        const path = process.getBuiltinModule("path"), physical = await fs.realpath(file), owned = new WeakSet<object>();
        fs.open = async (...args: Parameters<typeof fs.open>) => {
          const handle = await open(...args);
          if (args[1] === "wx" && path.dirname(String(args[0])) === path.dirname(physical)) owned.add(handle);
          return handle;
        };
        let release!: () => void;
        const gate = new Promise<void>(resolve => { release = resolve; });
        const state = globalThis as typeof globalThis & { selectedSave: { hits: number; release: () => void } };
        state.selectedSave = { hits: 0, release: () => { fs.writeFile = original; fs.open = open; release(); } };
        fs.writeFile = async (...args: Parameters<typeof fs.writeFile>) => {
          if (String(args[0]) === file || String(args[0]) === physical || (typeof args[0] === "object" && owned.has(args[0]))) {
            const first = ++state.selectedSave.hits === 1;
            if (first) {
              await gate;
              if (file.endsWith(".md")) throw Object.assign(new Error("Fixture save denied"), { code: "EACCES" });
            }
          }
          return original(...args);
        };
      }, path.join(source, file));
      await page.locator(".monaco-editor .view-lines").click();
      await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
      await page.keyboard.insertText("FIRST_UNACKNOWLEDGED_BUFFER");
      await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
      await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { selectedSave: { hits: number } }).selectedSave.hits)).toBeGreaterThan(0);
      await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
      await page.keyboard.insertText("PENDING_BUFFER_SURVIVES_REPAIR");
      await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
      await call(page, "ping", {});
      expect(await application.evaluate(() => (globalThis as typeof globalThis & { selectedSave: { hits: number } }).selectedSave.hits)).toBe(1);
    }
    renameSync(metadata, offline);
    const row = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).find(row => row && Number(row[2]) === application!.process().pid && row[3].includes("/agent/crab-desktop-agent "));
    if (!row) throw new Error("Owned native agent not found");
    process.kill(Number(row[1]), "SIGKILL");
    await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
    if (!pending) writeFileSync(path.join(source, file), value("AFTER_NATIVE_REPAIR"));
    renameSync(offline, metadata);
    const index = readFileSync(path.join(metadata, "index"));
    await page.getByRole("button", { name: "Retry reopening repository", exact: true }).click();
    await expect.poll(() => events.some(event => event.repo_id === handle.repo_id && event.native_session_id !== null && event.native_session_id !== handle.native_session_id)).toBe(true);
    await expect(content).toContainText(pending ? "PENDING_BUFFER_SURVIVES_REPAIR" : target === "LFS diff" ? digest("AFTER_NATIVE_REPAIR") : "AFTER_NATIVE_REPAIR");
    if (pending) {
      expect(readFileSync(path.join(source, file), "utf8")).toBe(value("BEFORE_NATIVE_REPAIR"));
      await application.evaluate(() => (globalThis as typeof globalThis & { selectedSave: { release: () => void } }).selectedSave.release());
      if (target === "pending Markdown") {
        const failure = page.getByRole("alert").filter({ hasText: "Could not save this file." });
        await expect(failure).toBeVisible();
        await failure.getByText("Error details", { exact: true }).click();
        await expect(failure.getByText(/Fixture save denied/)).toBeVisible();
        await failure.getByText("Error details", { exact: true }).click();
        await expect(content).toContainText("PENDING_BUFFER_SURVIVES_REPAIR");
        expect(readFileSync(path.join(source, file), "utf8")).toBe(value("BEFORE_NATIVE_REPAIR"));
        await page.screenshot({ path: test.info().outputPath("save-failure-retained-buffer.png") });
        await page.getByRole("button", { name: "Retry saving", exact: true }).click();
      }
      await expect.poll(() => readFileSync(path.join(source, file), "utf8")).toBe("PENDING_BUFFER_SURVIVES_REPAIR");
      await expect(page.getByLabel("Unsaved changes", { exact: true })).toHaveCount(0);
    }
    if (target.endsWith("diff")) await expect(page.getByRole("combobox", { name: "Compare file versions", exact: true })).toHaveValue("unstaged");
    expect(readFileSync(path.join(source, file)).equals(Buffer.from(pending ? "PENDING_BUFFER_SURVIVES_REPAIR" : value("AFTER_NATIVE_REPAIR")))).toBe(true);
    expect(readFileSync(path.join(metadata, "index")).equals(index)).toBe(true);
    await page.screenshot({ path: test.info().outputPath("selected-source-repaired.png") });
    if (target === "Changes file") {
      await page.getByTitle("Close file view and return to diff", { exact: true }).click();
      const row = page.getByRole("listbox", { name: "Changed files", exact: true }).getByRole("option").filter({ has: page.getByText(file, { exact: true }) });
      await row.click(); await row.hover();
      await page.getByRole("button", { name: `Stage ${file}`, exact: true }).click();
      await expect.poll(() => git(["show", `:${file}`]).toString()).toBe(value("AFTER_NATIVE_REPAIR"));
      await expect(page.locator("diffs-container")).toContainText("AFTER_NATIVE_REPAIR");
    }
    writeFileSync(test.info().outputPath("selected-source-proof.json"), JSON.stringify({ target, sameLogicalId: true, newNativeOwner: true, selectionPreserved: true, recaptured: true, readsPreservedContentAndIndex: true, acknowledgedLatestBuffer: pending, repairedStaging: target === "Changes file", events }, null, 2));
  } finally {
    if (existsSync(offline)) renameSync(offline, metadata);
    if (existsSync(path.join(source, file))) writeFileSync(test.info().outputPath("file-final.bin"), readFileSync(path.join(source, file)));
    if (application) {
      if (pending) await application.evaluate(() => (globalThis as typeof globalThis & { selectedSave?: { release: () => void } }).selectedSave?.release()).catch(() => {});
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.evaluate(({ dialog }) => { dialog.showMessageBoxSync = () => 1; }).catch(() => {});
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
