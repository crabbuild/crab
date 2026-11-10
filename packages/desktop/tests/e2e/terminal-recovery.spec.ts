import { test, expect, _electron as electron } from "@playwright/test";
import { createServer } from "vite";
import { createRequire } from "node:module";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import * as path from "node:path";

const desktop = path.resolve(__dirname, "../..");
const runtimeRequire = createRequire(path.join(desktop, "package.json"));

test("real terminal components recover processes and nested layouts across owners and app replacement", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(tmpdir(), "crab-term-e2e-"));
  const server = await createServer({ root: desktop, server: { host: "127.0.0.1", port: 0, strictPort: false } });
  await server.listen();
  const url = server.resolvedUrls?.local[0];
  if (!url) throw new Error("Terminal fixture renderer did not start");
  const args = [path.join(desktop, "tests/fixtures/terminal-recovery/main.cjs"), "--fixture-root", root, "--renderer-url", url];
  const launch = (extra: string[] = []) => electron.launch({ executablePath: runtimeRequire("electron"), args: [...args, ...extra] });
  let app = await launch();
  const proof = path.join(root, "foreground.jsonl");
  const records = () => existsSync(proof) ? readFileSync(proof, "utf8").trim().split("\n").map(line => JSON.parse(line)) : [];
  try {
    let page = await app.firstWindow();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(1);
    const quote = (value: string) => process.platform === "win32" ? `"${value}"` : `'${value.replace(/'/g, "'\\''")}'`;
    await page.locator(".xterm-helper-textarea:visible").first().focus();
    await page.keyboard.type([process.execPath, path.join(desktop, "tests/fixtures/terminal-recovery/agent.cjs"), proof].map(quote).join(" "));
    await page.keyboard.press("Enter");
    await expect.poll(() => records().find(value => value.input === "ready")?.pid).toBeTruthy();
    const pid = records()[0].pid;
    await page.getByRole("button", { name: "Split right", exact: false }).first().click();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(2);
    await page.getByRole("button", { name: "Split down", exact: false }).first().click();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(3);
    const handles = page.getByRole("separator");
    await handles.first().focus();
    await page.keyboard.press("ArrowLeft");
    await page.keyboard.press("ArrowLeft");
    await expect(handles.first()).toHaveAttribute("aria-valuenow", "30");
    await page.getByRole("button", { name: "Switch repository", exact: true }).click();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(1);
    await page.getByRole("button", { name: "Switch repository", exact: true }).click();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(3);
    await page.getByRole("button", { name: "Keep 2 renderers", exact: true }).click();
    await page.getByRole("button", { name: "Switch workspace", exact: true }).click();
    await expect(page.getByText("Active: b/one", { exact: false })).toBeVisible();
    await page.getByRole("button", { name: "Switch workspace", exact: true }).click();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(3);
    await page.locator(".xterm-helper-textarea:visible").first().focus();
    await page.keyboard.type("after-workspace");
    await page.keyboard.press("Enter");
    await expect.poll(() => records().find(value => value.input === "after-workspace")?.pid).toBe(pid);
    await app.close();
    const owner = createHash("sha256").update(`${root}/a/one`).digest("hex");
    const stateDir = path.join(root, "profile/terminal/layouts", owner);
    const { layout } = JSON.parse(readFileSync(path.join(stateDir, "state.json"), "utf8"));
    // An unreadable optional checkpoint must not discard this layout or its
    // live process identities when the upgraded app restores the profile.
    const checkpoint = path.join(stateDir, "scrollback", `${layout.tabs[0].id}.bin`);
    rmSync(checkpoint, { force: true });
    mkdirSync(checkpoint);
    const entry = path.join(root, "upgraded-daemon.cjs");
    writeFileSync(entry, `require(${JSON.stringify(path.join(desktop, "dist-electron/daemon/daemon-main.cjs"))});`);
    app = await launch(["--daemon-entry", entry]);
    page = await app.firstWindow();
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(3);
    await expect(page.getByRole("separator").first()).toHaveAttribute("aria-valuenow", "30");
    await page.locator(".xterm-helper-textarea:visible").first().focus();
    await page.keyboard.type("after-application-upgrade");
    await page.keyboard.press("Enter");
    await expect.poll(() => records().find(value => value.input === "after-application-upgrade")?.pid).toBe(pid);
    await page.screenshot({ path: test.info().outputPath("terminal-recovery.png") });
  } finally {
    await app.close();
    await server.close();
    const pidFile = path.join(root, "daemon/daemon-v2.pid");
    if (existsSync(pidFile)) {
      const { pid } = JSON.parse(readFileSync(pidFile, "utf8"));
      try { process.kill(pid, "SIGTERM"); } catch {}
    }
    rmSync(root, { recursive: true, force: true, maxRetries: 5 });
  }
});
