import { test, expect, _electron as electron } from "@playwright/test";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { StorageConnectionInput } from "../../electron/storage-connection-types";
import type { AgentEvent } from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";

interface Fixture {
  url: string;
  profiles: Array<StorageConnectionInput & { endpoint: string }>;
  payloads: Array<{ marker: string; sha256: string; size: number }>;
}

for (const kind of ["crab", "lfs"] as const) {
  const outcomes = kind === "crab" ? ["cancel", "native-crash", "app-crash", "cleanup", "budget", "missing", "corrupt"] as const : ["cancel", "native-crash", "app-crash", "missing", "corrupt"] as const;
  for (const outcome of outcomes) {
    const contentFailure = outcome === "missing" || outcome === "corrupt";
    test(outcome === "budget" ? "SDK identities and CLI share the saved desktop retention budget" : outcome === "cleanup" ? "SDK cache cleanup removes verified payloads and preserves retained state" : `SDK ${kind} ${contentFailure ? "content failure" : "partial response"} ${outcome} retries exact content`, async () => {
      const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
      test.skip(!descriptor || (outcome === "native-crash" && process.platform === "win32"), "Requires the owned storage fixture; native process lookup requires POSIX");
      test.setTimeout(120_000);
      const fixture = JSON.parse(readFileSync(descriptor!, "utf8")) as Fixture;
      const connection = fixture.profiles[0];
      const payload = fixture.payloads[0];
      const filename = kind === "crab" ? "weights.safetensors" : "lfs.safetensors";
      const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-sdk-recovery-"));
      const cache = path.join(root, "cache");
      const profile = path.join(root, "profile");
      const env: Record<string, string> = { ...Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), CRAB_CACHE_DIR: cache, AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(root, "gitconfig") };
      delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
      writeFileSync(env.GIT_CONFIG_GLOBAL, "");
      let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
      let stderr = "";
      const runtimes: Array<{ repo_id: string; native_session_id: string | null }> = [];
      const epoch = (repoId: string) => runtimes.filter(runtime => runtime.repo_id === repoId).at(-1)?.native_session_id;
      const transport = async (action: string) => {
        const response = await fetch(`${connection.endpoint}/__fixture_${action}`);
        expect(response.ok).toBe(true);
        return await response.json() as { get: number; writes: number; stream_blocked: number; streamed_bytes: number; paused: boolean; faulted: number };
      };
      const inventory = () => {
        const entries: Array<{ path: string; bytes: number }> = [];
        const visit = (directory: string) => {
          for (const entry of readdirSync(directory, { withFileTypes: true })) {
            const file = path.join(directory, entry.name);
            if (entry.isDirectory()) visit(file);
            else entries.push({ path: path.relative(cache, file).split(path.sep).join("/"), bytes: statSync(file).size });
          }
        };
        visit(cache);
        return entries;
      };
      const launch = async (welcome = false) => {
        application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env });
        application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
        const page = await application.firstWindow();
        await page.exposeFunction("recordSdkRuntime", (runtime: typeof runtimes[number]) => runtimes.push(runtime));
        await page.evaluate(() => {
          const target = window as typeof window & { recordSdkRuntime: (runtime: unknown) => Promise<void> };
          window.crab.onEvent((event: AgentEvent) => { if (event.method === "repo:capabilities-changed") void target.recordSdkRuntime(event.params); });
        });
        if (welcome) {
          await page.getByRole("button", { name: "Skip", exact: true }).click();
          await page.getByRole("button", { name: "No thanks", exact: true }).click();
          return page;
        }
        // A first-run modal can cover the shell after an interrupted session.
        // Wait beneath it, then dismiss the accessible prompt before acting.
        await page.getByRole("button", { name: "Activity", exact: true, includeHidden: true }).waitFor();
        for (const name of ["Skip", "No thanks"]) {
          const button = page.getByRole("button", { name, exact: true });
          if (await button.isVisible()) await button.click();
        }
        return page;
      };
      const openSnapshot = async (page: Awaited<ReturnType<typeof launch>>, selected = connection) => {
        await page.getByRole("button", { name: "More actions", exact: true }).click();
        await page.getByRole("menuitem", { name: "Open storage snapshot", exact: true }).click();
        const dialog = page.getByRole("dialog", { name: "Open storage snapshot", exact: true });
        await dialog.getByLabel("Repository URL", { exact: true }).fill(fixture.url);
        await dialog.getByRole("button", { name: `Edit ${selected.name}`, exact: true }).locator("..").getByRole("button", { name: "Use connection", exact: true }).click();
        await dialog.getByRole("button", { name: "Open snapshot", exact: true }).click();
        await expect(dialog).toHaveCount(0);
      };
      try {
        let page = await launch(true);
        await call(page, "storage_connections_save", { connection });
        await openSnapshot(page);
        await expect(page.locator(`[role="treeitem"][data-item-path="${filename}"]`)).toBeVisible();
        let repoId = await page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"));
        expect(repoId).toBeTruthy();
        await expect.poll(() => Boolean(epoch(repoId!))).toBe(true);
        const initialEpoch = epoch(repoId!);
        if (contentFailure) {
          const before = await transport(`${outcome}_${kind}`);
          await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
          const message = outcome === "missing" ? /repository content was not found/ : /content failed integrity verification/;
          const visibleError = page.getByRole("main").getByText(message).first();
          await expect(visibleError).toBeVisible();
          await expect(page.getByText(payload.marker, { exact: false })).toHaveCount(0);
          const failedRead = await page.evaluate(async params => {
            try { await window.crab.rpc("read_bytes", params); return null; }
            catch (error) { return error as { message?: string; code?: unknown; data?: unknown }; }
          }, { repo_id: repoId, path: filename, max_bytes: payload.size + 1 });
          expect(failedRead).toMatchObject({ code: 1001, data: { kind: "sdk", code: outcome === "missing" ? "not_found" : "corruption" } });
          const failed = await transport("stats");
          expect(failed.faulted).toBeGreaterThan(before.faulted);
          await page.screenshot({ animations: "disabled", path: test.info().outputPath("sdk-content-error.png") });
          await transport("clear_fault");
          await page.getByRole("button", { name: "Close tab", exact: true }).click();
          await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
          await expect(page.getByText(payload.marker, { exact: false }).first()).toBeVisible();
          const content = await call(page, "read_bytes", { repo_id: repoId, path: filename, max_bytes: payload.size + 1 });
          const bytes = Buffer.from(String(content.bytes_b64), "base64");
          expect({ size: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") }).toEqual({ size: payload.size, sha256: payload.sha256 });
          const after = await transport("stats");
          expect(after.writes).toBe(0);
          await page.screenshot({ animations: "disabled", path: test.info().outputPath("sdk-content-repaired.png") });
          writeFileSync(test.info().outputPath("sdk-content-failure-proof.json"), JSON.stringify({ kind, outcome, before, failed, failedRead, after, exactBytesAfterRepair: true }, null, 2));
          return;
        }
        if (outcome === "budget") {
          await expect(page.getByText("Read-only storage snapshot", { exact: true })).toBeVisible();
          await expect(page.getByRole("button", { name: "Refresh hydration status", exact: true })).toHaveCount(0);
          const payloads = () => inventory().filter(entry => /^namespaces\/[a-f0-9]{64}\/(?:chunks|xorbs|shards|ref-transactions|git-packs)\//.test(entry.path));
          const showExact = async (expected: typeof payload) => {
            await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
            await expect(page.getByText(expected.marker, { exact: false }).first()).toBeVisible();
            const content = await call(page, "read_bytes", { repo_id: repoId, path: filename, max_bytes: expected.size + 1 });
            expect(createHash("sha256").update(Buffer.from(String(content.bytes_b64), "base64")).digest("hex")).toBe(expected.sha256);
            await page.getByRole("button", { name: "Close tab", exact: true }).click();
          };
          const settings = async () => {
            await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Settings", exact: true }).click();
            await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
            return page.getByRole("region", { name: "Cache usage", exact: true });
          };
          await showExact(payload);
          await call(page, "storage_connections_save", { connection: fixture.profiles[1] });
          await openSnapshot(page, fixture.profiles[1]);
          repoId = await page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"));
          await showExact(fixture.payloads[1]);
          const before = payloads();
          expect(new Set(before.map(entry => entry.path.split("/")[1])).size).toBe(2);
          expect(inventory().filter(entry => entry.path.endsWith(".catalog.sqlite")).map(entry => entry.path)).toEqual([".catalog.sqlite"]);
          const retained = path.join(cache, "desktop-content", "database-query", "retained.bin");
          mkdirSync(path.dirname(retained), { recursive: true, mode: 0o700 });
          writeFileSync(retained, "retain query state", { mode: 0o600 });
          const usageBefore = await call(page, "cache_stats", { repo_id: repoId });
          let section = await settings();
          await section.getByRole("combobox", { name: "Cache retention", exact: true }).click();
          await page.getByRole("option", { name: "Limit disk retention", exact: true }).click();
          await section.getByLabel("Cache limit in MiB", { exact: true }).fill("0");
          await section.getByRole("button", { name: "Save budget", exact: true }).click();
          await expect(section.getByRole("status")).toContainText("Budget saved.");
          await expect(section.getByRole("button", { name: "Refresh usage", exact: true })).toBeEnabled();
          expect(payloads()).toEqual([]);
          expect(readFileSync(retained, "utf8")).toBe("retain query state");
          const zero = await call(page, "cache_stats", { repo_id: repoId });
          expect({ available: zero.budget_available, budget: zero.budget_bytes, over: zero.over_budget }).toEqual({ available: true, budget: 0, over: true });
          const beforeAllocated = (usageBefore.observed as { allocated_bytes: number }).allocated_bytes;
          const afterAllocated = (zero.observed as { allocated_bytes: number }).allocated_bytes;
          expect(afterAllocated).toBeLessThan(beforeAllocated);
          await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
          await showExact(fixture.payloads[1]);
          expect(payloads()).toEqual([]);
          section = await settings();
          await section.getByLabel("Cache limit in MiB", { exact: true }).fill("64");
          await section.getByRole("button", { name: "Save budget", exact: true }).click();
          await expect(section.getByRole("status")).toContainText("Budget saved.");
          await application!.close();
          application = undefined;
          page = await launch();
          section = await settings();
          await expect(section.getByLabel("Cache limit in MiB", { exact: true })).toHaveValue("64");
          const resources = await application!.evaluate(() => process.resourcesPath);
          const cli = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
          const external = JSON.parse(execFileSync(cli, ["cache", "budget", "--max-bytes", "0", "--json"], { cwd: root, env, encoding: "utf8" })) as { data: { usage: { budget_bytes: number } } };
          expect(external.data.usage.budget_bytes).toBe(0);
          await section.getByRole("button", { name: "Refresh usage", exact: true }).click();
          await expect(section.getByLabel("Cache limit in MiB", { exact: true })).toHaveValue("0");
          await section.getByRole("button", { name: "Save budget", exact: true }).scrollIntoViewIfNeeded();
          await page.screenshot({ animations: "disabled", path: test.info().outputPath("shared-cache-retention.png") });
          await section.screenshot({ animations: "disabled", path: test.info().outputPath("shared-cache-controls.png") });
          await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
          repoId = await page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"));
          await showExact(fixture.payloads[1]);
          expect(payloads()).toEqual([]);
          expect((await transport("stats")).writes).toBe(0);
          const second = await (await fetch(fixture.profiles[1].endpoint + "/__fixture_stats")).json() as { writes: number };
          expect(second.writes).toBe(0);
          writeFileSync(test.info().outputPath("shared-cache-budget-proof.json"), JSON.stringify({ before, usageBefore, zero, beforeAllocated, afterAllocated, externalCliObservedByLiveDesktop: true, exactReadsAtZero: true, savedBudgetSurvivesRestart: true, retainedStatePreserved: true }, null, 2));
          return;
        }
        if (outcome === "cleanup") {
          await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
          await expect(page.getByText(payload.marker, { exact: false }).first()).toBeVisible();
          await call(page, "read_bytes", { repo_id: repoId, path: filename, max_bytes: payload.size + 1 });
          await page.getByRole("button", { name: "Close tab", exact: true }).click();
          // SDK payloads include decoded ranges, whose filenames are not chunk
          // hashes. Count those alongside the shard before checking exact removal.
          const cached = inventory().filter(entry => /^namespaces\/[a-f0-9]{64}\/(?:chunks|xorbs|shards|ref-transactions|git-packs)\//.test(entry.path));
          expect(cached.length).toBeGreaterThan(0);
          const retained = path.join(cache, "desktop-content", "database-query", "retained.bin");
          mkdirSync(path.dirname(retained), { recursive: true, mode: 0o700 });
          writeFileSync(retained, "retain query state", { mode: 0o600 });
          const nav = page.getByRole("navigation", { name: "Primary", exact: true });
          await nav.getByRole("button", { name: "Settings", exact: true }).click();
          await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
          const settings = page.getByRole("region", { name: "Cache usage", exact: true });
          await settings.getByRole("button", { name: "Preview cleanup", exact: true }).click();
          await expect(settings.getByRole("status")).toContainText(`Would remove ${cached.length} payloads,`);
          expect(cached.every(entry => existsSync(path.join(cache, entry.path)))).toBe(true);
          await settings.getByRole("button", { name: "Clean cache", exact: true }).click();
          await expect(settings.getByRole("status")).toContainText(`Removed ${cached.length} payloads,`);
          await expect(settings.getByRole("button", { name: "Refresh usage", exact: true })).toBeEnabled();
          await expect(settings.getByRole("alert")).toHaveCount(0);
          expect(cached.some(entry => existsSync(path.join(cache, entry.path)))).toBe(false);
          expect(readFileSync(retained, "utf8")).toBe("retain query state");
          expect(existsSync(path.join(cache, ".catalog.sqlite"))).toBe(true);
          expect(inventory().some(entry => entry.path.startsWith("namespaces/") && entry.path.endsWith("/.catalog.sqlite"))).toBe(false);
          await settings.getByRole("status").scrollIntoViewIfNeeded();
          await page.screenshot({ animations: "disabled", path: test.info().outputPath("sdk-cache-cleanup.png") });
          const remaining = inventory();
          await nav.getByRole("button", { name: "Files", exact: true }).click();
          await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
          await expect(page.getByText(payload.marker, { exact: false }).first()).toBeVisible();
          const content = await call(page, "read_bytes", { repo_id: repoId, path: filename, max_bytes: payload.size + 1 });
          expect(createHash("sha256").update(Buffer.from(String(content.bytes_b64), "base64")).digest("hex")).toBe(payload.sha256);
          expect((await transport("stats")).writes).toBe(0);
          writeFileSync(test.info().outputPath("sdk-cleanup-proof.json"), JSON.stringify({ cached, retainedStatePreserved: true, catalogPreserved: true, remaining, exactContentAfterCleanup: true }, null, 2));
          return;
        }
        const before = await transport(`stream_${kind}`);
        await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
        await expect.poll(async () => (await transport("stats")).stream_blocked).toBeGreaterThan(0);
        expect((await transport("stats")).streamed_bytes).toBeGreaterThan(before.streamed_bytes);
        const interruptedCache = inventory();
        if (outcome === "cancel") {
          await page.getByRole("button", { name: "Close tab", exact: true }).click();
          await expect.poll(async () => (await transport("stats")).stream_blocked).toBe(0);
        } else if (outcome === "native-crash") {
          const parent = application!.process().pid;
          const native = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" })
            .split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line))
            .find(row => row && Number(row[2]) === parent && /\/agent\/crab-desktop-agent(?:\s|$)/.test(row[3]));
          if (!native) throw new Error("Owned native process was not found");
          process.kill(Number(native[1]), "SIGKILL");
        } else {
          const closed = application!.waitForEvent("close");
          process.kill(application!.process().pid!, "SIGKILL");
          await closed;
          application = undefined;
          await expect.poll(async () => (await transport("stats")).stream_blocked).toBe(0);
        }
        expect((await transport("stats")).paused).toBe(true);
        await transport("release");
        if (!application) page = await launch();
        if (!await page.getByRole("navigation", { name: "Primary", exact: true }).isVisible()) await openSnapshot(page);
        if (outcome === "native-crash") {
          const retry = page.getByRole("button", { name: "Retry reopening repository", exact: true });
          await expect.poll(async () => Boolean(epoch(repoId!) && epoch(repoId!) !== initialEpoch) || await retry.isVisible()).toBe(true);
          if (await retry.isVisible()) await retry.click();
          await expect.poll(() => Boolean(epoch(repoId!) && epoch(repoId!) !== initialEpoch)).toBe(true);
        }
        const row = page.locator(`[role="treeitem"][data-item-path="${filename}"]`);
        if (!await row.isVisible()) {
          const nav = page.getByRole("navigation", { name: "Primary", exact: true });
          await nav.getByRole("button", { name: "Settings", exact: true }).click();
          await nav.getByRole("button", { name: "Files", exact: true }).click();
        }
        await row.click();
        await expect(page.getByText(payload.marker, { exact: false }).first()).toBeVisible({ timeout: 20_000 });
        repoId = await page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"));
        const content = await call(page, "read_bytes", { repo_id: repoId, path: filename, max_bytes: payload.size + 1 });
        const bytes = Buffer.from(String(content.bytes_b64), "base64");
        expect({ size: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") }).toEqual({ size: payload.size, sha256: payload.sha256 });
        const after = await transport("stats");
        expect(after.get).toBeGreaterThan(before.get);
        expect(after.writes).toBe(0);
        await page.screenshot({ animations: "disabled", path: test.info().outputPath("sdk-recovered.png") });
        writeFileSync(test.info().outputPath("sdk-cache-proof.json"), JSON.stringify({ kind, outcome, before, after, interruptedCache, recoveredCache: inventory(), exactBytes: true }, null, 2));
      } catch (error) {
        const failedPage = application?.windows()[0];
        if (failedPage && !failedPage.isClosed()) {
          await failedPage.screenshot({ path: test.info().outputPath("sdk-recovery-failure.png") }).catch(() => {});
          await failedPage.locator("body").innerText().then(text => writeFileSync(test.info().outputPath("sdk-recovery-failure.txt"), text)).catch(() => {});
        }
        if (existsSync(cache)) writeFileSync(test.info().outputPath("sdk-recovery-failure-inventory.json"), JSON.stringify(inventory(), null, 2));
        throw error;
      } finally {
        try { await transport("clear_fault"); await transport("release"); }
        finally { await application?.close(); }
        writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
        rmSync(root, { recursive: true, force: true });
      }
    });
  }
}
