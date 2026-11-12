import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { cpSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import * as path from "node:path";
import { tmpdir } from "node:os";
import { call, packagedExecutable } from "./packaged-app";
import type { FileTransferActivity, FileTransferJob, FileTransferSource } from "../../electron/file-transfers";
import type { StorageConnectionInput } from "../../electron/storage-connection-types";
import type { RepoHandle } from "../../src/ipc";

interface Fixture {
  repositories: string[];
  profiles: Array<StorageConnectionInput & { endpoint: string }>;
  payloads: Array<{ sha256: string }>;
  cold_payloads: Array<{ sha256: string }>;
}
interface Transport { stream_blocked: number; streamed_bytes: number; writes: number }

for (const kind of ["crab", "lfs"] as const) for (const outcome of ["cancel", "native-crash", "app-crash", "cleanup-failure", "receipt-gap", "native-receipt-gap"] as const) {
  const publishedKind = kind === "crab" ? "lfs" : "crab";
  const scenario = outcome === "receipt-gap" ? `${publishedKind} main publication receipt loss` : outcome === "native-receipt-gap" ? `${publishedKind} native publication receipt loss` : `${kind} partial stream ${outcome}`;
  test(`${scenario} preserves publication receipts and explicitly recovers`, async () => {
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
    const versions = files.map((file, position) => {
      const pointer = git(["show", `:${file}`]).toString();
      const kind = pointer.includes("oid sha256:") ? "lfs" : "crab";
      const hash = /^(?:oid sha256:|file-hash )([0-9a-f]{64})$/m.exec(pointer)?.[1];
      const size = /^size (\d+)$/m.exec(pointer)?.[1];
      if (!hash || !size) throw new Error(`Fixture pointer identity missing for ${file}`);
      return { position, path: file, kind, hash, size: Number(size), mode: null } satisfies FileTransferSource;
    });
    const sha = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
    const transport = async (action: string) => await (await fetch(`${fixture.profiles[0].endpoint}/__fixture_${action}`)).json() as Transport;
    const artifacts = () => readdirSync(source).filter(name => name.startsWith(".crab-publication-") && name !== ".crab-publication-user-file");
    let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
    let stderr = "";
    const killNative = () => {
      const parent = application!.process().pid;
      const rows = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n");
      const native = rows.map(row => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(row)).find(row => row && Number(row[2]) === parent && /\/agent\/crab-desktop-agent(?:\s|$)/.test(row[3]));
      if (!native) throw new Error("Owned native writer was not found");
      process.kill(Number(native[1]), "SIGKILL");
    };
    const releaseJournal = async () => application?.evaluate(() => {
      const state = globalThis as typeof globalThis & { publicationJournal?: import("better-sqlite3").Database };
      if (state.publicationJournal) {
        state.publicationJournal.exec("ROLLBACK");
        state.publicationJournal.close();
        delete state.publicationJournal;
      }
    });
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
      const sources = async (jobId: string) => await call(page, "activity_file_sources", { job_id: jobId, start: 0, limit: 50 }) as unknown as FileTransferSource[];
      const showSources = async (jobId: string) => {
        expect(await sources(jobId)).toEqual(versions);
        await page.getByRole("button", { name: "hydrate · repository", exact: true }).first().click();
        await page.getByText("Captured source versions", { exact: true }).scrollIntoViewIfNeeded();
        for (const source of versions) await expect(page.getByText(source.hash, { exact: true })).toBeVisible();
        await page.screenshot({ animations: "disabled", path: test.info().outputPath("captured-sources.png") });
      };
      const restart = async () => {
        application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env });
        application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
        page = await application.firstWindow();
        // A first-run modal can cover the shell after an interrupted session.
        // Wait beneath it, then dismiss the accessible prompt before acting.
        await page.getByRole("button", { name: "Activity", exact: true, includeHidden: true }).waitFor();
        // A hard kill can lose Chromium preferences/recent tabs. Durable Activity
        // must support fresh retry from an empty start as well as a restored tab.
        if (await page.getByRole("button", { name: "Skip", exact: true }).isVisible()) await page.getByRole("button", { name: "Skip", exact: true }).click();
        if (await page.getByRole("button", { name: "No thanks", exact: true }).isVisible()) await page.getByRole("button", { name: "No thanks", exact: true }).click();
        await page.getByRole("button", { name: "Activity", exact: true }).click();
      };
      const before = await transport(`stream_${outcome === "native-receipt-gap" ? publishedKind : kind}`);
      await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Large Files", exact: true }).click();
      await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
      await page.getByRole("tab", { name: "Custom", exact: true }).click();
      for (const file of files) await page.getByRole("checkbox", { name: new RegExp(file) }).check();
      let lostReceipt: Promise<void> | undefined;
      if (outcome === "receipt-gap") {
        lostReceipt = application.waitForEvent("close").then(() => {});
        await application.evaluate(({ app }) => {
          const modules = process.getBuiltinModule("node:module") as typeof import("node:module");
          const require = modules.createRequire(`${app.getAppPath()}/package.json`);
          const { FileTransfers } = require(`${app.getAppPath()}/dist-electron/file-transfers.cjs`) as typeof import("../../electron/file-transfers");
          const observe = FileTransfers.prototype.observe;
          FileTransfers.prototype.observe = function (method, value) {
            const receipt = value as { repo_id?: unknown; files_published?: unknown } | null;
            // Native emits this frame only after the first real publication.
            // Kill this owned main before its SQLite observer records the frame.
            if (method === "progress/transfer" && receipt?.repo_id === "recovery" && receipt.files_published === 1) process.kill(process.pid, "SIGKILL");
            observe.call(this, method, value);
          };
        });
      }
      await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
      let original: FileTransferJob;
      if (lostReceipt) {
        await lostReceipt;
        expect(sha(readFileSync(path.join(source, files[0])))).toBe(hashes[0]);
        expect(readFileSync(path.join(source, files[1]))).toEqual(pointer);
        await restart();
        original = (await history()).jobs[0];
        expect(original).toMatchObject({ status: "interrupted", intent: { paths: files }, transfer: { files_published: 0 } });
        expect(original.nativeSessionId).toBeUndefined();
        await expect(page.getByText("Last recorded progress", { exact: true })).toBeInViewport();
        await expect(page.getByText("Outcome unknown. Check the working files.", { exact: true })).toBeInViewport();
      } else if (outcome === "native-receipt-gap") {
        await expect.poll(async () => (await transport("stats")).stream_blocked).toBeGreaterThan(0);
        expect((await transport("stats")).streamed_bytes).toBeGreaterThan(before.streamed_bytes);
        await expect.poll(async () => (await history()).jobs.find(job => job.status === "running")?.transfer?.received_bytes ?? 0).toBeGreaterThan(0);
        original = (await history()).jobs.find(job => job.status === "running")!;
        // Hold the real journal writer after payload allocation. Publication
        // can rename the file, but finish cannot send its receipt until cleanup
        // commits. This gives a deterministic native crash between those steps.
        await application.evaluate(({ app }, journal) => {
          const modules = process.getBuiltinModule("node:module") as typeof import("node:module");
          const require = modules.createRequire(`${app.getAppPath()}/package.json`);
          const Database = require("better-sqlite3") as typeof import("better-sqlite3");
          const db = new Database(journal);
          db.exec("BEGIN IMMEDIATE");
          (globalThis as typeof globalThis & { publicationJournal?: import("better-sqlite3").Database }).publicationJournal = db;
        }, path.join(source, ".git", "crab-desktop-publication.sqlite3"));
        await transport("release");
        await expect.poll(() => sha(readFileSync(path.join(source, files[0]))), { intervals: [20, 50, 100] }).toBe(hashes[0]);
        expect((await history()).jobs.find(job => job.job_id === original.job_id)?.transfer?.files_published).toBe(0);
        await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.transfer?.received_bytes ?? 0).toBeGreaterThan(0);
        original = (await history()).jobs.find(job => job.job_id === original.job_id)!;
        killNative();
        await releaseJournal();
        await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.status).toBe("interrupted");
        await page.getByRole("button", { name: "Activity", exact: true }).click();
        await expect(page.getByText("Outcome unknown. Check the working files.", { exact: true })).toBeInViewport();
      } else {
        await expect.poll(async () => (await transport("stats")).stream_blocked).toBeGreaterThan(0);
        expect((await transport("stats")).streamed_bytes).toBeGreaterThan(before.streamed_bytes);
        await expect.poll(() => sha(readFileSync(path.join(source, files[0])))).toBe(hashes[0]);
        expect(readFileSync(path.join(source, files[1]))).toEqual(pointer);
        await expect.poll(async () => (await history()).jobs.find(job => job.status === "running")?.transfer?.files_published).toBe(1);
        original = (await history()).jobs.find(job => job.status === "running")!;
        expect(original.intent.paths).toEqual(files);
        await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.transfer?.received_bytes ?? 0).toBeGreaterThan(0);
        original = (await history()).jobs.find(job => job.job_id === original.job_id)!;
        expect(artifacts()).toHaveLength(1);
        const status = await call(page, "git_status_porcelain", { repo_id: "recovery" });
        expect((status.entries as Array<{ path: string }>).some(entry => artifacts().some(name => entry.path.startsWith(name + "/")))).toBe(false);
        expect((status.entries as Array<{ path: string }>).some(entry => entry.path === ".crab-publication-user-file")).toBe(true);
        await page.getByRole("button", { name: "Activity", exact: true }).click();
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
          await showSources(original.job_id);
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
          if (outcome === "native-crash") killNative();
          else {
            const exited = application.waitForEvent("close"); application.process().kill("SIGKILL"); await exited;
            await restart();
          }
          await expect.poll(async () => (await history()).jobs.find(job => job.job_id === original.job_id)?.status).toBe("interrupted");
        }
      }
      await expect.poll(async () => (await transport("stats")).stream_blocked).toBe(0);
      expect(sha(readFileSync(path.join(source, files[0])))).toBe(hashes[0]);
      expect(readFileSync(path.join(source, files[1]))).toEqual(pointer);
      expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
      await page.screenshot({ animations: "disabled", path: test.info().outputPath("interrupted-transfer.png") });
      await showSources(original.job_id);
      const interrupted = (await history()).jobs.find(job => job.job_id === original.job_id)!;
      if (outcome === "receipt-gap" && interrupted.transfer?.received_bytes === undefined) {
        await expect(page.getByText("Storage received: not recorded", { exact: true })).toBeVisible();
      } else {
        expect(interrupted.transfer?.received_bytes).toBeGreaterThanOrEqual(original.transfer?.received_bytes ?? 0);
        await expect(page.getByText(/received from storage$/).first()).toBeVisible();
      }
      await transport("release");
      if (outcome !== "app-crash" && outcome !== "receipt-gap") {
        await expect.poll(() => page.evaluate(() => window.crab.getCapabilities("recovery")).then(caps => caps.canHydrateSelection)).toBe(true);
        const recoveredStatus = await call(page, "git_status_porcelain", { repo_id: "recovery" });
        expect((recoveredStatus.entries as Array<{ path: string }>).some(entry => artifacts().some(name => entry.path.startsWith(name + "/")))).toBe(false);
      }
      await application.evaluate(({ dialog }) => { dialog.showMessageBox = async () => ({ response: 1, checkboxChecked: false }); });
      await page.getByRole("button", { name: "Retry transfer", exact: true }).first().click();
      await expect.poll(() => sha(readFileSync(path.join(source, files[1]))), { timeout: 40_000 }).toBe(hashes[1]);
      await expect.poll(async () => (await history()).jobs.some(job => job.job_id !== original.job_id && job.status === "done" && job.intent.paths.length === 1 && job.intent.paths[0] === files[1])).toBe(true);
      const retry = (await history()).jobs.find(job => job.job_id !== original.job_id && job.status === "done" && job.intent.paths.length === 1 && job.intent.paths[0] === files[1])!;
      expect(retry.transfer?.received_bytes).toBeGreaterThanOrEqual(0);
      expect((await history()).jobs.find(job => job.job_id === original.job_id)?.transfer?.received_bytes).toBe(interrupted.transfer?.received_bytes);
      expect(await sources(retry.job_id)).toEqual([{ ...versions[1], position: 0 }]);
      expect(await sources(original.job_id)).toEqual(versions);
      expect(artifacts()).toHaveLength(0);
      expect(readFileSync(path.join(source, ".crab-publication-user-file"), "utf8")).toBe("retain this user file\n");
      expect(git(["ls-files", "--stage", "-z"])).toEqual(index);
      expect((await transport("stats")).writes).toBe(0);
      await page.screenshot({ animations: "disabled", path: test.info().outputPath("retried-transfer.png") });
      writeFileSync(test.info().outputPath("recovery-proof.json"), JSON.stringify({ kind, publishedKind, outcome, original: original.job_id, lostPublicationReceipt: outcome === "receipt-gap" || outcome === "native-receipt-gap", firstPublishedFilePreserved: true, pointerPreservedDuringInterruption: true, exactRemainingBytes: true, unrelatedIndexPreserved: true, onlyOwnedScratchRemoved: true, history: await history() }, null, 2));
    } finally {
      await transport("release");
      await releaseJournal()?.catch(() => {});
      await application?.close();
      writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
      rmSync(root, { recursive: true, force: true });
    }
  });
}
