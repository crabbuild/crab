import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { createServer } from "vite";
import { createRequire } from "node:module";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";

const desktop = path.resolve(__dirname, "../..");
const runtimeRequire = createRequire(path.join(desktop, "package.json"));

async function stopFixtureDaemons(root: string) {
  const runtimes = path.join(root, "daemon/runtimes");
  const directories = [path.join(root, "daemon"), ...(existsSync(runtimes) ? readdirSync(runtimes).map(name => path.join(runtimes, name)) : [])];
  for (const directory of directories) {
    const pidFile = path.join(directory, "daemon-v2.pid");
    if (!existsSync(pidFile)) continue;
    const { pid } = JSON.parse(readFileSync(pidFile, "utf8"));
    try { process.kill(pid, "SIGTERM"); } catch {}
    await expect.poll(() => {
      try { process.kill(pid, 0); return false; }
      catch (error) { return (error as NodeJS.ErrnoException).code === "ESRCH"; }
    }, { timeout: 10_000 }).toBe(true);
  }
}

test("terminal controls keep normal and daemon lifetimes distinct with shared input and recovery", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(tmpdir(), "crab-term-controls-"));
  const server = await createServer({ root: desktop, cacheDir: path.join(root, "vite-cache"), server: { host: "127.0.0.1", port: 0, strictPort: false } });
  await server.listen();
  const url = server.resolvedUrls?.local[0];
  if (!url) throw new Error("Terminal fixture renderer did not start");
  const launch = (mode: string) => electron.launch({
    executablePath: runtimeRequire("electron"),
    args: [path.join(desktop, "tests/fixtures/terminal-recovery/main.cjs"), "--fixture-root", root, "--renderer-url", url],
    env: { ...process.env, CRAB_PTY_MODE: mode },
  });
  let app = await launch("daemon");
  const proof = (name: string) => path.join(root, `${name}.jsonl`);
  const records = (name: string): Array<{ pid: number; input: string }> => existsSync(proof(name))
    ? readFileSync(proof(name), "utf8").trim().split("\n").map(line => JSON.parse(line)) : [];
  const quote = (value: string) => process.platform === "win32" ? `"${value}"` : `'${value.replace(/'/g, "'\\''")}'`;
  async function startAgent(page: Page, name: string) {
    await page.locator(".xterm:visible").last().click();
    await expect(page.locator(".xterm-helper-textarea:visible").last()).toBeFocused();
    await page.keyboard.type([process.execPath, path.join(desktop, "tests/fixtures/terminal-recovery/agent.cjs"), proof(name)].map(quote).join(" "));
    await page.keyboard.press("Enter");
    await expect.poll(() => records(name).find(value => value.input === "ready")?.pid).toBeTruthy();
    return records(name)[0].pid;
  }
  try {
    let page = await app.firstWindow();
    await expect(page.locator('[data-session-type="daemon"]')).toHaveCount(1);
    await expect(page.getByRole("img", { name: "Daemon terminal", exact: true }).locator("svg.lucide-send-to-back")).toBeVisible();
    for (const [name, tooltip] of [["New terminal", "New terminal (⌘T)"], ["Find", "Find in terminal (⌘F)"], ["Terminal sessions", "Terminal sessions"]]) {
      const button = page.getByRole("button", { name, exact: true });
      await expect(button).toHaveText("");
      await button.focus();
      await expect(page.getByRole("tooltip")).toContainText(tooltip);
    }
    const daemonPid = await startAgent(page, "daemon");
    const mode = page.getByRole("button", { name: "Terminal mode for new tabs and splits", exact: true });
    await mode.click();
    await page.getByRole("menuitemradio", { name: /^Normal terminal/ }).press("Enter");
    await expect(mode).toBeFocused();
    await expect(mode).toHaveText("");
    await expect(mode).toHaveAttribute("aria-description", /^Normal terminal/);
    await page.getByRole("button", { name: "New terminal", exact: true }).click();
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(1);
    const indicator = page.locator('[data-session-type="in-process"]').getByRole("img", { name: "Normal terminal", exact: true });
    await expect(indicator).toHaveText("");
    await indicator.hover();
    await expect(page.getByRole("tooltip")).toContainText("ends when its tab or Crab closes");
    const normalPid = await startAgent(page, "normal");
    const broadcast = page.getByRole("button", { name: /^Broadcast input to all terminals/ });
    await broadcast.click();
    await expect(page.getByRole("status")).toContainText("Input is sent to all 2 open terminals");
    await page.locator(".xterm-helper-textarea:visible").focus();
    await page.keyboard.type("both-modes");
    await page.keyboard.press("Enter");
    await expect.poll(() => ["normal", "daemon"].map(name => records(name).find(value => value.input === "both-modes")?.pid)).toEqual([normalPid, daemonPid]);
    await broadcast.click();
    await page.getByRole("button", { name: "Find", exact: true }).click();
    const search = page.getByPlaceholder("Search terminal…");
    await search.fill("both-modes");
    await search.press("Enter");
    await expect(page.locator(".xterm-selection div").first()).toBeVisible();
    await search.press("Escape");
    const settings = page.getByRole("button", { name: "Terminal settings", exact: true });
    await settings.click();
    const font = page.getByRole("button", { name: "Terminal font settings", exact: true });
    await font.click();
    await page.keyboard.press("Escape");
    await expect(font).toBeFocused();
    await page.screenshot({ path: test.info().outputPath("terminal-controls-wide.png") });
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setContentSize(640, 820));
    const controls = page.getByRole("group", { name: "Terminal controls", exact: true });
    expect(await controls.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
    for (const button of await controls.getByRole("button").all()) {
      const box = await button.boundingBox();
      expect(box && box.height === 28 && box.width >= 28).toBeTruthy();
    }
    for (const icon of await controls.locator("button svg").all()) {
      const box = await icon.boundingBox();
      expect(box && box.width === 14 && box.height === 14).toBeTruthy();
    }
    expect((await page.locator('[data-session-type="in-process"]').first().boundingBox())?.height).toBe(32);
    await page.screenshot({ path: test.info().outputPath("terminal-controls-narrow.png") });
    await settings.click();
    await page.getByRole("button", { name: "Hide daemon terminal", exact: true }).click();
    await expect(page.locator('[data-session-type="daemon"]')).toHaveCount(0);
    expect(() => process.kill(daemonPid, 0)).not.toThrow();
    await page.getByRole("button", { name: "Terminal sessions", exact: true }).click();
    const sessions = page.getByRole("complementary", { name: "Terminal sessions", exact: true });
    await sessions.getByRole("button", { name: "Attach", exact: true }).click();
    await expect(page.locator('[data-session-type="daemon"]')).toHaveCount(1);
    await expect(sessions).toBeHidden();
    await page.locator(".xterm-helper-textarea:visible").focus();
    await page.keyboard.type("after-attach");
    await page.keyboard.press("Enter");
    await expect.poll(() => records("daemon").find(value => value.input === "after-attach")?.pid).toBe(daemonPid);
    await page.getByRole("button", { name: /^Split right/ }).click();
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(2);
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(2);
    await app.close();
    await expect.poll(() => {
      try { process.kill(normalPid, 0); return true; } catch { return false; }
    }).toBe(false);
    expect(() => process.kill(daemonPid, 0)).not.toThrow();

    // No daemon connection: normal tabs must still restore as fresh shells.
    app = await launch("in-process");
    page = await app.firstWindow();
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(2);
    await expect(page.locator(".xterm-helper-textarea:visible")).toHaveCount(2);
    await page.locator(".xterm-helper-textarea:visible").last().focus();
    await page.keyboard.type(`echo normal-restored > ${quote(path.join(root, "restored.txt"))}`);
    await page.keyboard.press("Enter");
    await expect.poll(() => existsSync(path.join(root, "restored.txt")) && readFileSync(path.join(root, "restored.txt"), "utf8").trim()).toBe("normal-restored");
    await page.getByRole("button", { name: "Terminal mode for new tabs and splits", exact: true }).click();
    await expect(page.getByRole("menuitemradio", { name: /^Daemon terminal/ })).toBeDisabled();
    await page.keyboard.press("Escape");
    const restoredPid = await startAgent(page, "restored-agent");
    await page.getByRole("button", { name: "Close terminal", exact: true }).last().click();
    const confirmation = page.getByRole("alertdialog", { name: /A command is still running/ });
    await confirmation.getByRole("button", { name: "Cancel", exact: true }).click();
    expect(() => process.kill(restoredPid, 0)).not.toThrow();
    await page.getByRole("button", { name: "Close terminal", exact: true }).last().click();
    await confirmation.getByRole("button", { name: "Confirm", exact: true }).click();
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(1);
    await expect.poll(() => {
      try { process.kill(restoredPid, 0); return true; } catch { return false; }
    }).toBe(false);
  } finally {
    await app.close();
    await server.close();
    await stopFixtureDaemons(root);
    rmSync(root, { recursive: true, force: true, maxRetries: 5 });
  }
});


test("CLI presets show host availability and execute configured paths", async () => {
  test.skip(process.platform === "win32", "The owned CLI fixture uses a POSIX script");
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(tmpdir(), "crab-host-cli-e2e-"));
  const bin = path.join(root, "outside PATH");
  mkdirSync(bin);
  const binary = path.join(bin, "claude");
  const proof = path.join(root, "launched.txt");
  writeFileSync(binary, '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "fixture 1.0.0"; else printf "%s\\n" "$@" > "$CLI_PROOF"; printf "host-cli-%s\\n" launched; fi\n', { mode: 0o755 });
  const settings = path.join(root, "agent-settings.json");
  const config = { binaryPath: binary, extraArgs: ["saved argument"], env: { CLI_PROOF: proof } };
  const save = (overrides: object) => writeFileSync(settings, JSON.stringify({ orchestratorAgentConfigs: { "claude-code": { ...config, ...overrides } } }));
  save({});
  const server = await createServer({ root: desktop, cacheDir: path.join(root, "vite-cache"), server: { host: "127.0.0.1", port: 0, strictPort: false } });
  await server.listen();
  const url = server.resolvedUrls?.local[0];
  if (!url) throw new Error("Terminal fixture renderer did not start");
  const app = await electron.launch({ executablePath: runtimeRequire("electron"), args: [path.join(desktop, "tests/fixtures/terminal-recovery/main.cjs"), "--fixture-root", root, "--renderer-url", url, "--fixture-cli-catalog"] });
  try {
    const page = await app.firstWindow();
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(1);
    await expect(page.getByRole("button", { name: "Terminal mode for new tabs and splits", exact: true })).toHaveAttribute("aria-description", /^Normal terminal/);
    const menu = page.getByRole("button", { name: "Terminal presets", exact: true });
    await menu.click();
    await expect(page.getByRole("menuitem", { name: "Claude Code", exact: true })).toBeVisible();
    await expect(page.getByRole("menuitem", { name: "Aider", exact: true })).toHaveCount(0);
    await page.getByRole("menuitem", { name: "Claude Code", exact: true }).press("ArrowRight");
    await page.getByRole("menuitem", { name: "Resume last session", exact: true }).press("Enter");
    await expect.poll(() => existsSync(proof) && readFileSync(proof, "utf8")).toBe("saved argument\n--resume\n");
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(2);
    await expect(page.locator(".xterm-rows").last()).toContainText("host-cli-launched");
    await menu.click();
    await expect(page.getByRole("menuitem", { name: "Claude Code", exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("host-cli-menu.png") });
    save({ binaryPath: path.join(bin, "missing") });
    await page.getByRole("menuitem", { name: "Refresh CLI list", exact: true }).click();
    await expect(page.getByRole("menuitem", { name: "Claude Code", exact: true })).toBeVisible();
    await page.getByRole("menuitem", { name: "Claude Code", exact: true }).press("ArrowRight");
    await page.getByRole("menuitem", { name: "Launch Claude Code", exact: false }).press("Enter");
    await expect(page.getByText("Cannot launch agent CLI", { exact: true })).toBeVisible();
    await expect(page.locator('[data-session-type="in-process"]')).toHaveCount(2);
    save({ enabled: false });
    await menu.click();
    await expect(page.getByRole("menu").getByRole("status")).toContainText("No agent CLIs found");
  } finally {
    await app.close();
    await server.close();
    await stopFixtureDaemons(root);
    rmSync(root, { recursive: true, force: true, maxRetries: 5 });
  }
});
