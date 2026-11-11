import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, renameSync, rmSync } from "node:fs";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";
import type { ChunkDiffMetrics } from "../../src/ipc";

test("path staging preserves literal selection and safely unstages an unborn repository", async () => {
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-staging-paths-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  const git = (args: string[]) => execFileSync("git", ["--literal-pathspecs", ...args], { cwd: source, stdio: "pipe" }).toString();
  git(["init", "--initial-branch=main"]);
  for (const file of ["data[1].txt", "data1.txt"]) writeFileSync(path.join(source, file), file);
  const application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`] });
  try {
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await call(page, "open_local", { repo_id: "staging-paths", path: source });
    await call(page, "git_add_paths", { repo_id: "staging-paths", paths: ["data[1].txt"] });
    expect(git(["ls-files"]).trim()).toBe("data[1].txt");
    await call(page, "git_add_paths", { repo_id: "staging-paths", paths: ["."] });
    const before = git(["ls-files", "--stage"]);
    const error = await page.evaluate(async () => {
      try { await window.crab.rpc("git_reset_paths", { repo_id: "staging-paths", paths: [] }); return null; }
      catch (error) { return error instanceof Error ? error.message : JSON.stringify(error); }
    });
    expect(error).toContain("Select at least one file to unstage");
    expect(git(["ls-files", "--stage"])).toBe(before);
    await call(page, "git_reset_paths", { repo_id: "staging-paths", paths: ["data[1].txt"] });
    expect(git(["ls-files"]).trim()).toBe("data1.txt");
    await call(page, "git_reset_paths", { repo_id: "staging-paths", paths: ["."] });
    expect(git(["ls-files"])).toBe("");
    for (const file of ["data[1].txt", "data1.txt"]) expect(readFileSync(path.join(source, file), "utf8")).toBe(file);
  } finally {
    await application.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("Crab and LFS UI lifecycle preserves bytes and staging across edits, rename and removal", async () => {
  test.skip(!process.env.CRAB_SDK_TEST_S3_BUCKET, "Requires the owned writable S3-compatible fixture");
  test.setTimeout(180_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-lifecycle-"));
  const source = path.join(root, "source");
  const clone = path.join(root, "clone");
  const files = ["model[1].bin", "model-lfs.bin"];
  const content = randomBytes(8 * 1024 ** 2);
  const edited = Buffer.from(content);
  randomBytes(4096).copy(edited, 4 * 1024 ** 2);
  const digest = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
  const profile = path.join(root, "profile");
  const env = { ...Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), PATH: `${path.dirname(crab)}${path.delimiter}${process.env.PATH ?? ""}`, CRAB_CACHE_DIR: path.join(root, "source-cache") };
  const appEnv = { ...env, AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1" };
  const git = (cwd: string, args: string[]) => execFileSync("git", ["--literal-pathspecs", ...args], { cwd, env, stdio: "pipe" });
  const xorbs = () => {
    // StoreLayout's canonical xorb prefix excludes LFS objects and Git metadata.
    const listing = JSON.parse(execFileSync("aws", ["--endpoint-url", process.env.AWS_ENDPOINT_URL!, "s3api", "list-objects-v2", "--bucket", process.env.CRAB_SDK_TEST_S3_BUCKET!, "--prefix", ".crab/xorbs/"], { env, stdio: "pipe" }).toString()) as { Contents?: Array<{ Key: string; Size: number }> };
    return new Map((listing.Contents ?? []).map(object => [object.Key, object.Size]));
  };
  mkdirSync(source);
  git(source, ["init", "--initial-branch=main"]);
  git(source, ["config", "user.name", "Desktop fixture"]);
  git(source, ["config", "user.email", "desktop@example.invalid"]);
  git(source, ["config", "commit.gpgsign", "false"]);
  const url = `crab://${process.env.CRAB_SDK_TEST_S3_BUCKET}/desktop/${randomUUID()}`;
  execFileSync(crab, ["init", url], { cwd: source, env, stdio: "pipe" });
  writeFileSync(path.join(source, ".gitattributes"), "model-lfs.bin filter=lfs -text\n");
  writeFileSync(path.join(source, "other.txt"), "original\n");
  writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
  git(source, ["add", "--", ".gitattributes", "crab.toml", ".gitignore", "other.txt"]);
  git(source, ["commit", "-m", "Repository fixture"]);
  writeFileSync(path.join(source, "other.txt"), "retain this staged change\n");
  git(source, ["add", "--", "other.txt"]);
  const unrelated = git(source, ["ls-files", "--stage", "-z", "--", "other.txt"]);
  for (const file of files) writeFileSync(path.join(source, file), content);
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let failed = false;
  try {
    application = await electron.launch({ executablePath, args: [`--user-data-dir=${profile}`], env: appEnv });
    let page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const openSource = async () => {
      const handle = await call(page, "open_local", { repo_id: "lifecycle-source", path: source });
      await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    };
    const openClone = async () => {
      const handle = await call(page, "open_local", { repo_id: "lifecycle-clone", path: clone });
      await page.evaluate(({ handle, clone }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: clone } })), { handle, clone });
    };
    await openSource();
    const primary = () => page.getByRole("navigation", { name: "Primary", exact: true });
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    const connections = page.getByRole("region", { name: "Storage connections", exact: true });
    await connections.getByRole("button", { name: "Add connection", exact: true }).click();
    await connections.getByLabel("Connection name", { exact: true }).fill("Lifecycle storage");
    await connections.getByLabel("Bucket", { exact: true }).fill(process.env.CRAB_SDK_TEST_S3_BUCKET!);
    await connections.getByLabel("Signing region", { exact: true }).fill(process.env.AWS_REGION!);
    await connections.getByLabel("Endpoint URL (optional)", { exact: true }).fill(process.env.AWS_ENDPOINT_URL!);
    await connections.getByLabel("Access key ID", { exact: true }).fill(process.env.AWS_ACCESS_KEY_ID!);
    await connections.getByLabel("Secret access key", { exact: true }).fill(process.env.AWS_SECRET_ACCESS_KEY!);
    await connections.getByRole("button", { name: "Save connection", exact: true }).click();
    await connections.getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(connections.getByRole("status")).toContainText("Connection selected for this repository");
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Track & Stage", exact: true }).click();
    for (const file of files) await page.getByText(file, { exact: true }).locator("../..").getByRole("checkbox").check();
    await page.getByRole("button", { name: "Track 2 files", exact: true }).click();
    await page.getByRole("button", { name: "Track 2 files", exact: true }).click();
    await expect(page.getByText("Ready to commit", { exact: true })).toBeVisible();
    await page.getByLabel("Commit message for tracked files", { exact: true }).fill("Publish both large-file formats");
    await page.getByRole("button", { name: "Commit", exact: true }).click();
    await expect(page.getByText(/^Committed/)).toBeVisible();
    const first = git(source, ["rev-parse", "HEAD"]).toString().trim();
    expect(git(source, ["show", "HEAD:model-lfs.bin"]).toString()).toContain(`oid sha256:${digest(content)}`);
    const pointers = files.map(file => git(source, ["show", `HEAD:${file}`]));
    expect(git(source, ["ls-files", "--stage", "-z", "--", "other.txt"])).toEqual(unrelated);
    await page.getByRole("button", { name: /^Push(?: \d+ commits?)? to origin$/ }).click();
    await expect(page.getByText("Push complete", { exact: true })).toBeVisible({ timeout: 40_000 });
    const firstXorbs = xorbs();
    expect(firstXorbs.size).toBeGreaterThan(0);
    await page.screenshot({ path: test.info().outputPath("ui-lifecycle-published.png") });

    await page.getByTitle("Clone Repository", { exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Clone Repository", exact: true });
    await dialog.getByRole("button", { name: "Crab Storage", exact: true }).click();
    await dialog.getByPlaceholder("crab://bucket/repo", { exact: true }).fill(url);
    await dialog.locator('input[placeholder^="/path/to/parent/"]').fill(clone);
    await dialog.getByRole("button", { name: "Use connection", exact: true }).click();
    await dialog.getByRole("button", { name: "Clone", exact: true }).click();
    await expect(dialog).toHaveCount(0, { timeout: 40_000 });
    expect(git(clone, ["rev-parse", "HEAD"]).toString().trim()).toBe(first);
    for (let index = 0; index < files.length; index++) expect(readFileSync(path.join(clone, files[index]))).toEqual(pointers[index]);
    await page.screenshot({ path: test.info().outputPath("ui-lifecycle-lazy-clone.png") });

    // A fresh application/cache proves reconstruction cannot use source staging.
    await application.close();
    application = await electron.launch({ executablePath, args: [`--user-data-dir=${profile}`], env: { ...appEnv, CRAB_CACHE_DIR: path.join(root, "cold-clone-cache") } });
    page = await application.firstWindow();
    await expect(page.getByRole("button", { name: "Repository overview", exact: true })).toContainText("clone", { timeout: 30_000 });
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
    for (const file of files) await expect.poll(() => digest(readFileSync(path.join(clone, file))), { timeout: 40_000 }).toBe(digest(content));
    expect(git(clone, ["status", "--porcelain", "--", ...files]).toString()).toBe("");
    await page.screenshot({ path: test.info().outputPath("ui-lifecycle-cold-hydration.png") });

    for (const file of files) writeFileSync(path.join(source, file), edited);
    await openSource();
    await primary().getByRole("button", { name: "Git", exact: true }).click();
    for (const file of files) {
      const row = page.getByText(file, { exact: true }).and(page.locator(":visible")).first();
      await row.hover();
      const previous = git(source, ["rev-parse", `:${file}`]).toString();
      await page.getByRole("button", { name: `Stage ${file}`, exact: true }).click();
      await expect.poll(() => git(source, ["rev-parse", `:${file}`]).toString(), { timeout: 30_000 }).not.toBe(previous);
    }
    const diff = await call(page, "file_diff", { repo_id: "lifecycle-source", path: files[0], before: { kind: "revision", rev: first }, after: { kind: "index" } });
    expect(diff.status).toBe("modified");
    expect(diff.chunk_metrics).not.toBeNull();
    const metrics = diff.chunk_metrics as ChunkDiffMetrics;
    expect(metrics.unchanged_bytes).toBeGreaterThan(content.length * .8);
    expect(metrics.added_bytes).toBeGreaterThan(0);
    expect(metrics.added_bytes).toBeLessThan(content.length * .2);
    await test.info().attach("small-edit-chunk-reuse", { body: JSON.stringify(metrics), contentType: "application/json" });
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Track & Stage", exact: true }).click();
    await page.getByPlaceholder("Commit message", { exact: true }).and(page.locator(":visible")).fill("Publish the small edit");
    await page.getByRole("button", { name: "Refresh file list", exact: true }).click();
    await expect(page.getByPlaceholder("Commit message", { exact: true }).and(page.locator(":visible"))).toHaveValue("Publish the small edit");
    await page.getByRole("button", { name: "Commit", exact: true }).click();
    await expect.poll(() => git(source, ["rev-parse", "HEAD"]).toString().trim()).not.toBe(first);
    expect(git(source, ["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "HEAD"]).toString().split("\0").filter(Boolean)).toEqual([files[0]]);
    expect(git(source, ["ls-files", "--stage", "-z", "--", "other.txt"])).toEqual(unrelated);
    // The Crab staging section commits chunks only. The normal composer owns
    // the remaining staged LFS edit, so explicitly unstage the sibling first.
    await primary().getByRole("button", { name: "Git", exact: true }).click();
    await page.getByText("other.txt", { exact: true }).and(page.locator(":visible")).first().hover();
    await page.getByRole("button", { name: "Unstage other.txt", exact: true }).click();
    await page.getByRole("textbox", { name: "Commit subject", exact: true }).fill("Publish the LFS edit");
    const crabCommit = git(source, ["rev-parse", "HEAD"]).toString();
    await page.getByRole("button", { name: "Commit staged changes", exact: true }).click();
    await expect.poll(() => git(source, ["rev-parse", "HEAD"]).toString()).not.toBe(crabCommit);
    const second = git(source, ["rev-parse", "HEAD"]).toString().trim();
    expect(git(source, ["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "HEAD"]).toString().split("\0").filter(Boolean)).toEqual([files[1]]);
    await page.getByText("other.txt", { exact: true }).and(page.locator(":visible")).first().hover();
    await page.getByRole("button", { name: "Stage other.txt", exact: true }).click();
    await expect.poll(() => git(source, ["ls-files", "--stage", "-z", "--", "other.txt"]).toString()).toBe(unrelated.toString());
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Track & Stage", exact: true }).click();
    await page.getByRole("toolbar", { name: "Large Files", exact: true }).getByRole("button", { name: "Push", exact: true }).click();
    await page.getByRole("button", { name: /^Push(?: \d+ commits?)? to origin$/ }).click();
    await expect(page.getByText("Push complete", { exact: true })).toBeVisible({ timeout: 40_000 });
    const newXorbs = [...xorbs()].filter(([key]) => !firstXorbs.has(key));
    const storedAddedBytes = newXorbs.reduce((sum, [, bytes]) => sum + bytes, 0);
    expect(storedAddedBytes).toBeGreaterThan(0);
    expect(storedAddedBytes).toBeLessThan(content.length * .2);
    writeFileSync(test.info().outputPath("xorb-dedup-proof.json"), JSON.stringify({ originalBytes: content.length, editedBytes: 4096, metrics, newXorbs, storedAddedBytes }));
    await page.getByRole("button", { name: "Done", exact: true }).click();

    await openClone();
    await primary().getByRole("button", { name: "Git", exact: true }).click();
    await page.getByRole("button", { name: "Branches", exact: true }).click();
    await page.getByRole("button", { name: "Fetch all remotes", exact: true }).click();
    await expect.poll(() => git(clone, ["rev-parse", "origin/main"]).toString().trim(), { timeout: 30_000 }).toBe(second);
    await page.getByRole("button", { name: "Pull main", exact: true }).click();
    await page.getByRole("menuitem", { name: /Fast-forward only$/ }).click();
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect.poll(() => git(clone, ["rev-parse", "HEAD"]).toString().trim(), { timeout: 30_000 }).toBe(second);
    for (const file of files) expect(git(clone, ["show", `HEAD:${file}`])).toEqual(git(source, ["show", `HEAD:${file}`]));
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
    for (const file of files) await expect.poll(() => digest(readFileSync(path.join(clone, file))), { timeout: 40_000 }).toBe(digest(edited));
    expect(git(clone, ["status", "--porcelain", "--", ...files]).length).toBe(0);
    expect(git(source, ["ls-files", "--stage", "-z", "--", "other.txt"])).toEqual(unrelated);
    await page.screenshot({ path: test.info().outputPath("ui-lifecycle-pulled-edit.png") });

    const beforeRenameXorbs = xorbs();
    const renamed = "renamed model[2].bin";
    renameSync(path.join(source, files[0]), path.join(source, renamed));
    rmSync(path.join(source, files[1]));
    await openSource();
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Track & Stage", exact: true }).click();
    await page.getByText(renamed, { exact: true }).and(page.locator(":visible")).locator("../..").getByRole("checkbox").check();
    await page.getByRole("button", { name: "Track 1 file", exact: true }).click();
    await page.getByRole("button", { name: "Track 1 file", exact: true }).click();
    await expect(page.getByText("Ready to commit", { exact: true })).toBeVisible();
    await page.getByRole("textbox", { name: "Commit message for tracked files", exact: true }).fill("Track the renamed large file");
    await page.getByRole("button", { name: "Commit", exact: true }).click();
    await expect(page.getByText(/^Committed/)).toBeVisible();
    expect(git(source, ["show", `HEAD:${renamed}`])).toEqual(git(source, ["show", `${second}:${files[0]}`]));
    expect(git(source, ["ls-files", "--stage", "-z", "--", "other.txt"])).toEqual(unrelated);
    await page.getByRole("button", { name: "Skip push — I'll push later", exact: true }).click();
    await primary().getByRole("button", { name: "Git", exact: true }).click();
    for (const file of files) {
      await page.getByText(file, { exact: true }).and(page.locator(":visible")).first().hover();
      await page.getByRole("button", { name: `Stage ${file}`, exact: true }).click();
      await expect.poll(() => git(source, ["ls-files", "--stage", "--", file]).length).toBe(0);
    }
    await expect(page.getByRole("button", { name: /Staged for Commit\s*3/ })).toBeVisible();
    await page.getByText("other.txt", { exact: true }).and(page.locator(":visible")).first().hover();
    await page.getByRole("button", { name: "Unstage other.txt", exact: true }).click();
    await page.getByRole("textbox", { name: "Commit subject", exact: true }).fill("Remove the previous large-file paths");
    const renamedCommit = git(source, ["rev-parse", "HEAD"]).toString();
    await page.getByRole("button", { name: "Commit staged changes", exact: true }).click();
    await expect.poll(() => git(source, ["rev-parse", "HEAD"]).toString()).not.toBe(renamedCommit);
    const third = git(source, ["rev-parse", "HEAD"]).toString().trim();
    expect(git(source, ["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "HEAD"]).toString().split("\0").filter(Boolean).sort()).toEqual([...files].sort());
    await page.getByText("other.txt", { exact: true }).and(page.locator(":visible")).first().hover();
    await page.getByRole("button", { name: "Stage other.txt", exact: true }).click();
    await expect.poll(() => git(source, ["ls-files", "--stage", "-z", "--", "other.txt"]).toString()).toBe(unrelated.toString());
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("toolbar", { name: "Large Files", exact: true }).getByRole("button", { name: "Push", exact: true }).click();
    await page.getByRole("button", { name: /^Push(?: \d+ commits?)? to origin$/ }).click();
    await expect(page.getByText("Push complete", { exact: true })).toBeVisible({ timeout: 40_000 });
    expect([...xorbs()].sort()).toEqual([...beforeRenameXorbs].sort());
    await page.getByRole("button", { name: "Done", exact: true }).click();
    await openClone();
    await primary().getByRole("button", { name: "Git", exact: true }).click();
    await page.getByRole("button", { name: "Branches", exact: true }).click();
    await page.getByRole("button", { name: "Fetch all remotes", exact: true }).click();
    await expect.poll(() => git(clone, ["rev-parse", "origin/main"]).toString().trim(), { timeout: 30_000 }).toBe(third);
    await page.getByRole("button", { name: "Pull main", exact: true }).click();
    git(clone, ["branch", "menu-refresh-proof"]);
    await expect(page.getByText("menu-refresh-proof", { exact: true }).and(page.locator(":visible"))).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("pull-menu-survives-refresh.png") });
    await page.getByRole("menuitem", { name: /Fast-forward only$/ }).click();
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect.poll(() => git(clone, ["rev-parse", "HEAD"]).toString().trim(), { timeout: 30_000 }).toBe(third);
    for (const file of files) expect(existsSync(path.join(clone, file))).toBe(false);
    expect(readFileSync(path.join(clone, renamed))).toEqual(git(source, ["show", `HEAD:${renamed}`]));
    await primary().getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await expect(page.getByRole("button", { name: /^Hydrate 1 file/ })).toBeEnabled();
    await page.getByRole("button", { name: /^Hydrate 1 file/ }).click();
    await expect.poll(() => digest(readFileSync(path.join(clone, renamed))), { timeout: 40_000 }).toBe(digest(edited));
    expect(git(clone, ["status", "--porcelain", "--", ...files, renamed]).length).toBe(0);
    expect(git(source, ["ls-files", "--stage", "-z", "--", "other.txt"])).toEqual(unrelated);
    await page.screenshot({ path: test.info().outputPath("ui-lifecycle-renamed-and-removed.png") });
  } catch (error) {
    failed = true;
    if (application) {
      const page = await application.firstWindow();
      await page.screenshot({ path: test.info().outputPath("ui-lifecycle-failure.png") });
      writeFileSync(test.info().outputPath("ui-lifecycle-failure.txt"), await page.locator("body").innerText());
    }
    throw error;
  } finally {
    await application?.close();
    if (!failed) rmSync(root, { recursive: true, force: true });
    else writeFileSync(test.info().outputPath("fixture-root.txt"), root);
  }
});
