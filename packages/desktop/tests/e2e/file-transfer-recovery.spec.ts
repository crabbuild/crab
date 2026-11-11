import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { cpSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";
import type { FileTransferActivity } from "../../electron/file-transfers";
import type { StorageConnectionInput } from "../../electron/storage-connection-types";
import type { RepoHandle } from "../../src/ipc";

interface Fixture {
  repositories: string[];
  profiles: Array<StorageConnectionInput & { endpoint: string }>;
  payloads: Array<{ sha256: string }>;
  cold_payloads: Array<{ sha256: string }>;
}
interface Transport { stream_blocked: number; streamed_bytes: number; writes: number }

for (const kind of ["crab", "lfs"] as const) for (const outcome of ["cancel", "native-crash", "app-crash", "cleanup-failure"] as const) {
  test(`${kind} partial stream ${outcome} preserves publication receipts and explicitly recovers`, async () => {
    test.skip(!process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE || process.platform === "win32", "Requires the owned storage and POSIX process fixtures");
    test.setTimeout(150_000);
    const fixture = JSON.parse(readFileSync(process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE!, "utf8")) as Fixture;
    const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-transfer-recovery-"));
    const source = path.join(root, "repository"), profile = path.join(root, "profile");
    cpSync(fixture.repositories[0], source, { recursive: true });
    const env = { ...Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1" };
    writeFileSync(env.GIT_CONFIG_GLOBAL, "");
    const git = (args: string[]) => execFileSync("git", ["--literal-pathspecs", ...args], { cwd: source, env });
    const pending = kind === "lfs" ? "cold.safetensors" : "weights.safetensors";
    const first = kind === "lfs" ? "weights.safetensors" : "cold.safetensors";
    const files = ["a-first.safetensors", "z-pending.safetensors"];
    const hashes = kind === "lfs" ? [fixture.payloads[0].sha256, fixture.cold_payloads[0].sha256] : [fixture.cold_payloads[0].sha256, fixture.payloads[0].sha256];
    for (const file of [first, pending]) writeFileSync(path.join(source, file), git(["show", `:${file}`]));
    git(["mv", "--", first, files[0]]); git(["mv", "--", pending, files[1]]);
    writeFileSync(path.join(source, ".gitattributes"), readFileSync(path.join(source, ".gitattributes"), "utf8") + `\n${files[0]} filter=${kind === "lfs" ? "crab" : "lfs"} -text\n${files[1]} filter=${kind} -text\n`);
    writeFileSync(path.join(source, "keep-staged.txt"), "original\n");
    git(["add", "--", ".gitattributes", "keep-staged.txt"]);
    git(["-c", "user.name=Desktop fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Recovery fixture paths"]);
    writeFileSync(path.join(source, "keep-staged.txt"), "retain unrelated staging\n"); git(["add", "--", "keep-staged.txt"]);
    writeFileSync(path.join(source, ".crab-publication-user-file"), "retain this user file\n");
    mkdirSync(path.join(source, ".crab"), { recursive: true });
    writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "chunks"))}\n`);
    const index = git(["ls-files", "--stage", "-z"]);
    const pointer = git(["show", `:${files[1]}`]);
    const sha = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
    const transport = async (action: string) => await (await fetch(`${fixture.profiles[0].endpoint}/__fixture_${action}`)).json() as Transport;
    const artifacts = () => readdirSync(source).filter(name => name.startsWith(".crab-publication-") && name !== ".crab-publication-user-file");
    let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
    let stderr = "";
    try {
      application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env });
      application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
      let page = await application.firstWindow();
      await page.getByRole("button", { name: "Skip", exact: true }).click();
      await page.getByRole("button", { name: "No thanks", exact: true }).click();
      const connection = await call(page, "storage_connections_save", { connection: fixture.profiles[0] });
      const handle = await call(page, "open_local", { repo_id: "recovery", path: source, storage_connection_id: connection.id }) as unknown as RepoHandle;
      await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
      const history = async () => await call(page, "activity_file_transfers", {}) as unknown as FileTransferActivity;
      const before = await transport(`stream_${kind}`);
      await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Large Files", exact: true }).click();
      await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
      await page.getByRole("tab", { name: "Custom", exact: true }).click();
      for (const file of files) await page.getByRole("checkbox", { name: new RegExp(file) }).check();
      await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
      await expect.poll(async () => (await transport("stats")).stream_blocked).toBeGreaterThan(0);
      expect((await transport("stats")).streamed_bytes).toBeGreaterThan(before.streamed_bytes);
      await expect.poll(() => sha(readFileSync(path.join(source, files[0])))).toBe(hashes[0]);
      expect(readFileSync(path.join(source, files[1]))).toEqual(pointer);
      await expect.poll(async () => (await history()).jobs.find(job => job.status === "running")?.transfer?.files_published).toBe(1);
      const original = (await history()).jobs.find(job => job.status === "running")!;
      expect(original.intent.paths).toEqual(files);
      expect(artifacts()).toHaveLength(1);
      const status = await call(page, "git_status_porcelain", { repo_id: "recovery" });
      expect((status.entries as Array<{ path: string }>).some(entry => artifacts().some(name => entry.path.startsWith(name + "/")))).toBe(false);
      expect((status.entries as Array<{ path: string }>).some(entry => entry.path === ".crab-publication-user-file")).toBe(true);
      await page.getByTitle("Toggle Activity (background jobs)", { exact: true }).click();
      await expect(page.getByText("1 of 2 files materialized", { exact: true })).toBeInViewport();
      if (outcome === "cleanup-failure") {
        const foreign = path.join(source, artifacts()[0], "foreign.txt");
        writeFileSync(foreign, "retain foreign content\n");
        await transport("release");
        await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.status).toBe("failed");
        for (let index = 0; index < files.length; index++) expect(sha(readFileSync(path.join(source, files[index])))).toBe(hashes[index]);
        expect((await history()).jobs.find(job => job.job_id === original.job_id)?.transfer?.files_published).toBe(2);
        await expect(page.getByText("2 of 2 files materialized", { exact: true })).toBeInViewport();
        await expect(page.getByText(/Publication scratch at .* was retained/).first()).toBeInViewport();
        expect(readFileSync(foreign, "utf8")).toBe("retain foreign content\n");
        expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
        await page.screenshot({ animations: "disabled", path: test.info().outputPath("cleanup-failure.png") });
        // Remove only the fixture's foreign file, then exercise the next real
        // publication. Recovery must dispose of the now-empty owned directory.
        rmSync(foreign);
        await page.getByRole("button", { name: "Close activity drawer", exact: true }).click();
        await application.evaluate(({ dialog }) => { dialog.showMessageBox = async () => ({ response: 1, checkboxChecked: false }); });
        await page.keyboard.press(process.platform === "darwin" ? "Meta+Shift+P" : "Control+Shift+P");
        const palette = page.getByRole("dialog", { name: "Command palette", exact: true });
        await palette.getByRole("combobox").fill("Crab: Dehydrate File");
        await palette.getByRole("option").filter({ hasText: "Crab: Dehydrate File" }).click();
        const fileInput = palette.getByPlaceholder("Exact file path to dehydrate…");
        await fileInput.fill(files[1]);
        await fileInput.press("Enter");
        await expect(palette).toHaveCount(0);
        await expect.poll(() => readFileSync(path.join(source, files[1])).equals(pointer)).toBe(true);
        await expect.poll(async () => (await history()).jobs.some(job => job.op === "dehydrate" && job.status === "done")).toBe(true);
        expect(artifacts()).toHaveLength(0);
        expect(readFileSync(path.join(source, ".crab-publication-user-file"), "utf8")).toBe("retain this user file\n");
        expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
        expect((await transport("stats")).writes).toBe(0);
        writeFileSync(test.info().outputPath("recovery-proof.json"), JSON.stringify({ kind, outcome, publishedBytesVerified: true, foreignFilePreserved: true, cleanupFailureVisible: true, nextPublicationRecovered: true, unrelatedIndexPreserved: true, history: await history() }, null, 2));
        return;
      }
      if (outcome === "cancel") {
        await page.getByRole("button", { name: "Cancel materialization", exact: true }).click();
        await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.status).toBe("cancelled");
        expect(artifacts()).toHaveLength(0);
      } else {
        const parent = application.process().pid;
        const rows = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n");
        const native = rows.map(row => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(row)).find(row => row && Number(row[2]) === parent && /\/agent\/crab-desktop-agent(?:\s|$)/.test(row[3]));
        if (!native) throw new Error("Owned native writer was not found");
        if (outcome === "native-crash") process.kill(Number(native[1]), "SIGKILL");
        else {
          const exited = application.waitForEvent("close"); application.process().kill("SIGKILL"); await exited;
          application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env });
          application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
          page = await application.firstWindow();
          await page.getByTitle("Toggle Activity (background jobs)", { exact: true }).waitFor();
          // Chromium preferences and recent tabs can be lost in a hard kill.
          // Activity must reopen from its durable intent even at an empty start.
          if (await page.getByRole("button", { name: "Skip", exact: true }).isVisible()) await page.getByRole("button", { name: "Skip", exact: true }).click();
          if (await page.getByRole("button", { name: "No thanks", exact: true }).isVisible()) await page.getByRole("button", { name: "No thanks", exact: true }).click();
          await page.getByTitle("Toggle Activity (background jobs)", { exact: true }).click();
        }
        await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.status).toBe("interrupted");
      }
      await expect.poll(async () => (await transport("stats")).stream_blocked).toBe(0);
      expect(sha(readFileSync(path.join(source, files[0])))).toBe(hashes[0]);
      expect(readFileSync(path.join(source, files[1]))).toEqual(pointer);
      expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
      await page.screenshot({ animations: "disabled", path: test.info().outputPath("interrupted-transfer.png") });
      await transport("release");
      if (outcome !== "app-crash") {
        await expect.poll(() => page.evaluate(() => window.crab.getCapabilities("recovery")).then(caps => caps.canHydrateSelection)).toBe(true);
        const recoveredStatus = await call(page, "git_status_porcelain", { repo_id: "recovery" });
        expect((recoveredStatus.entries as Array<{ path: string }>).some(entry => artifacts().some(name => entry.path.startsWith(name + "/")))).toBe(false);
      }
      await application.evaluate(({ dialog }) => { dialog.showMessageBox = async () => ({ response: 1, checkboxChecked: false }); });
      await page.getByRole("button", { name: "Retry transfer", exact: true }).first().click();
      await expect.poll(() => sha(readFileSync(path.join(source, files[1]))), { timeout: 40_000 }).toBe(hashes[1]);
      await expect.poll(async () => (await history()).jobs.some(job => job.job_id !== original.job_id && job.status === "done" && job.intent.paths.length === 1 && job.intent.paths[0] === files[1])).toBe(true);
      expect(artifacts()).toHaveLength(0);
      expect(readFileSync(path.join(source, ".crab-publication-user-file"), "utf8")).toBe("retain this user file\n");
      expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
      expect((await transport("stats")).writes).toBe(0);
      await page.screenshot({ animations: "disabled", path: test.info().outputPath("retried-transfer.png") });
      writeFileSync(test.info().outputPath("recovery-proof.json"), JSON.stringify({ kind, outcome, original: original.job_id, firstPublishedFilePreserved: true, pointerPreservedDuringInterruption: true, exactRemainingBytes: true, unrelatedIndexPreserved: true, onlyOwnedScratchRemoved: true, history: await history() }, null, 2));
    } finally {
      await transport("release");
      await application?.close();
      writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
      rmSync(root, { recursive: true, force: true });
    }
  });
}
