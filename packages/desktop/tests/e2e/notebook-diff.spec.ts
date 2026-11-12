import { chooseSelect } from "../helpers/select";
import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, openSync, ftruncateSync, closeSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";

test("packaged notebooks compare captured versions, page content and expose failures", async () => {
  test.setTimeout(120_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-notebook-diff-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  const notebook = (count: number, version: string) => JSON.stringify({
    nbformat: 4, nbformat_minor: 5, metadata: {},
    cells: Array.from({ length: count }, (_, index) => ({
      id: `cell-${index}`, cell_type: "code", metadata: { tags: index === 0 ? [version] : [] }, execution_count: null,
      source: index === 0 ? `value = '${version}'` : `unchanged_${index}`,
      outputs: index === 0 && version !== "committed" ? [{ output_type: "stream", name: "stdout", text: `<script>window.notebookExecuted=true</script>${version} ${"x".repeat(9000)} END-${version}` }] : [],
    })),
  });
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    git(["config", "user.name", "Notebook test"]);
    git(["config", "user.email", "test@example.invalid"]);
    const original = notebook(65, "committed");
    writeFileSync(path.join(source, "analysis.ipynb"), original);
    writeFileSync(path.join(source, "scalable.ipynb"), notebook(40_000, "committed"));
    for (const file of ["broken.ipynb", "empty.ipynb", "removed.ipynb"]) writeFileSync(path.join(source, file), notebook(1, "committed"));
    git(["add", "--all"]);
    git(["-c", "commit.gpgsign=false", "commit", "-m", "Notebook fixture"]);
    const staged = notebook(65, "staged");
    const working = notebook(65, "working");
    writeFileSync(path.join(source, "analysis.ipynb"), staged);
    git(["add", "analysis.ipynb"]);
    const index = git(["show", ":analysis.ipynb"]);
    writeFileSync(path.join(source, "analysis.ipynb"), working);
    writeFileSync(path.join(source, "scalable.ipynb"), notebook(40_000, "working"));
    writeFileSync(path.join(source, "broken.ipynb"), "not JSON");
    writeFileSync(path.join(source, "empty.ipynb"), "");
    rmSync(path.join(source, "removed.ipynb"));
    writeFileSync(path.join(source, "added.ipynb"), notebook(1, "new"));
    writeFileSync(path.join(source, "added-empty.ipynb"), notebook(0, "new"));
    writeFileSync(path.join(source, "missing.ipynb"), `version https://git-lfs.github.com/spec/v1\noid sha256:${createHash("sha256").update("missing").digest("hex")}\nsize 7\n`,
    );
    const large = openSync(path.join(source, "oversize.ipynb"), "w");
    try { ftruncateSync(large, 17 * 1024 ** 2); } finally { closeSync(large); }

    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env });
    const page = await application.firstWindow();
    const rendererErrors: string[] = [];
    page.on("pageerror", error => rendererErrors.push(error.message));
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return undefined; } }).toBeTruthy();
    const handle = await call(page, "open_local", { repo_id: "notebook-diff", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    expect(await call(page, "file_diff", { repo_id: "notebook-diff", path: "analysis.ipynb", before: { kind: "index" }, after: { kind: "working" } })).toEqual({ kind: "notebook", path: "analysis.ipynb", before: staged, after: working });
    const primary = page.getByRole("navigation", { name: "Primary", exact: true });
    const openDiff = async (file: string) => {
      await primary.getByRole("button", { name: "Git", exact: true }).click();
      await page.getByRole("navigation", { name: "Git", exact: true }).getByRole("button", { name: "Changes", exact: true }).click();
      await page.getByRole("radio", { name: "List", exact: true }).click();
      await page.getByRole("listbox", { name: "Changed files", exact: true }).getByRole("option").filter({ has: page.getByText(file, { exact: true }) }).dblclick();
    };
    await openDiff("analysis.ipynb");
    const selector = page.getByRole("combobox", { name: "Compare file versions", exact: true });
    const comparison = page.getByRole("region", { name: "Notebook comparison", exact: true });
    await expect(comparison.getByText("1 changed cell", { exact: true })).toBeVisible();
    await expect(comparison.getByRole("region", { name: "After Source", exact: true })).toContainText("working");
    await chooseSelect(selector, "staged");
    await expect(comparison.getByRole("region", { name: "After Source", exact: true })).toContainText("staged");
    await chooseSelect(selector, "unstaged");
    await expect(comparison.getByRole("region", { name: "Before Source", exact: true })).toContainText("staged");
    await expect(comparison.getByRole("region", { name: "After Source", exact: true })).toContainText("working");
    await comparison.getByRole("button", { name: "Show all outputs", exact: true }).click();
    const afterOutput = comparison.getByRole("region", { name: "After Outputs", exact: true });
    await expect(afterOutput).toContainText("window.notebookExecuted=true");
    await afterOutput.getByRole("button", { name: "Next after outputs page", exact: true }).click();
    await afterOutput.getByRole("button", { name: "Next after outputs page", exact: true }).click();
    await expect(afterOutput).toContainText("END-working");
    await page.mouse.move(1100, 20);
    await page.screenshot({ path: test.info().outputPath("notebook-captured-output-page.png") });
    await comparison.getByRole("button", { name: "Hide output diff", exact: true }).click();
    await expect(afterOutput).toHaveCount(0);
    await comparison.getByRole("button", { name: "Show output diff", exact: true }).click();
    await expect(afterOutput).toBeVisible();
    await comparison.getByRole("button", { name: "Hide all outputs", exact: true }).click();
    await expect(afterOutput).toHaveCount(0);
    await comparison.getByRole("button", { name: "Show unchanged cells", exact: true }).click();
    await expect(comparison.getByRole("article")).toHaveCount(25);
    await comparison.getByRole("button", { name: "Next notebook cells page", exact: true }).click();
    await expect(comparison.getByRole("article").first()).toHaveAttribute("aria-label", "unchanged cell 26");
    await comparison.getByRole("button", { name: "Next notebook cells page", exact: true }).click();
    await expect(comparison.getByRole("article")).toHaveCount(15);
    await expect(comparison.getByRole("article").last()).toHaveAttribute("aria-label", "unchanged cell 65");
    await page.screenshot({ path: test.info().outputPath("notebook-unchanged-cells-page.png") });
    expect(await page.evaluate(() => (window as unknown as { notebookExecuted?: boolean }).notebookExecuted)).toBeUndefined();
    for (const file of ["broken.ipynb", "empty.ipynb"]) {
      await openDiff(file);
      await expect(page.getByRole("alert")).toContainText("After notebook:");
      await expect(comparison).toHaveCount(0);
    }
    for (const file of ["missing.ipynb", "oversize.ipynb"]) {
      await openDiff(file);
      await expect(page.getByRole("alert")).toBeVisible();
      await expect(comparison).toHaveCount(0);
      if (file === "oversize.ipynb") await expect(page.getByRole("alert")).toContainText("16 MiB combined input limit");
    }
    await page.screenshot({ path: test.info().outputPath("notebook-explicit-read-limit.png") });
    await openDiff("scalable.ipynb");
    await expect(comparison.getByText("1 changed cell", { exact: true })).toBeVisible();
    await expect(comparison.getByText("• 39999 unchanged", { exact: true })).toBeVisible();
    await comparison.getByRole("button", { name: "Show unchanged cells", exact: true }).click();
    await comparison.getByRole("spinbutton", { name: "Notebook cells page", exact: true }).fill("1600");
    await expect(comparison.getByRole("article")).toHaveCount(25);
    await expect(comparison.getByRole("article").last()).toHaveAttribute("aria-label", "unchanged cell 40000");
    await page.screenshot({ path: test.info().outputPath("notebook-40000-cells-last-page.png") });
    const debuggerSession = await page.context().newCDPSession(page);
    let notebookWorker: string | undefined;
    const detachedWorkers = new Set<string>();
    debuggerSession.on("Target.attachedToTarget", event => {
      if (event.targetInfo.type === "worker" && event.targetInfo.url.includes("notebook-diff.worker")) notebookWorker = event.sessionId;
    });
    debuggerSession.on("Target.detachedFromTarget", event => detachedWorkers.add(event.sessionId));
    try {
      // Pause the real worker at startup so cancellation is deterministic even
      // on fast CI hosts. The UI must terminate it, not just hide its result.
      await debuggerSession.send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: true, flatten: true, filter: [{ type: "worker", exclude: false }] });
      await chooseSelect(selector, "staged");
      await expect(page.getByRole("button", { name: "Cancel notebook comparison", exact: true })).toBeVisible();
      await expect.poll(() => notebookWorker).toBeTruthy();
      await page.getByRole("button", { name: "Cancel notebook comparison", exact: true }).click();
      await expect(page.getByRole("alert")).toContainText("Notebook comparison cancelled.");
      await expect.poll(() => detachedWorkers.has(notebookWorker!)).toBe(true);
      await page.screenshot({ path: test.info().outputPath("notebook-cancelled-worker.png") });
    } finally {
      await debuggerSession.send("Target.setAutoAttach", { autoAttach: false, waitForDebuggerOnStart: false, flatten: true });
      await debuggerSession.detach();
    }
    await page.getByRole("button", { name: "Retry notebook comparison", exact: true }).click();
    await expect(comparison.getByText("0 changed cells", { exact: true })).toBeVisible();
    await openDiff("removed.ipynb");
    await expect(comparison.getByRole("article").first()).toHaveAttribute("aria-label", "removed cell 1");
    await expect(comparison.getByText("File deleted", { exact: true })).toBeVisible();
    await openDiff("added.ipynb");
    await expect(comparison.getByRole("article").first()).toHaveAttribute("aria-label", "added cell 1");
    await openDiff("added-empty.ipynb");
    await expect(comparison.getByText("File added", { exact: true })).toBeVisible();
    await expect(comparison.getByText("0 changed cells", { exact: true })).toBeVisible();
    await primary.getByRole("button", { name: "Git", exact: true }).click();
    await page.getByRole("navigation", { name: "Git", exact: true }).getByRole("button", { name: "History", exact: true }).click();
    await page.getByRole("listbox", { name: "Commits", exact: true }).getByText("Notebook fixture", { exact: true }).click();
    await expect(comparison.filter({ hasText: "65 changed cells" })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("notebook-initial-commit.png") });
    expect(git(["show", ":analysis.ipynb"]).equals(index)).toBe(true);
    expect(readFileSync(path.join(source, "analysis.ipynb"), "utf8")).toBe(working);
    expect(rendererErrors).toEqual([]);
  } catch (error) {
    const page = application?.windows()[0];
    if (page) {
      await page.screenshot({ path: test.info().outputPath("failure.png"), timeout: 2000 }).catch(() => undefined);
      await page.locator("body").innerText({ timeout: 2000 }).then(body => test.info().attach("failure-state", { body, contentType: "text/plain" })).catch(() => undefined);
    }
    throw error;
  } finally {
    let forced = false;
    const timer = setTimeout(() => { forced = true; application?.process().kill("SIGTERM"); }, 15_000);
    try {
      await application?.close();
    } finally {
      clearTimeout(timer);
      rmSync(root, { recursive: true, force: true });
    }
    if (forced) throw new Error("Packaged application did not exit cleanly within 15 seconds.");
  }
});
