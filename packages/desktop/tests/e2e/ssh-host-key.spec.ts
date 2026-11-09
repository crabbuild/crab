import { test, expect, _electron as electron } from "@playwright/test";
import { generateKeyPairSync, createHash } from "node:crypto";
import { once } from "node:events";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { Server, utils, type Connection } from "ssh2";
import { call, packagedExecutable } from "./packaged-app";

function keyPair() {
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs1", format: "pem" }, publicKeyEncoding: { type: "pkcs1", format: "pem" } });
  const key = utils.parseKey(privateKey);
  if (key instanceof Error) throw key;
  return { privateKey, key, fingerprint: "SHA256:" + createHash("sha256").update(key.getPublicSSH()).digest("base64").replace(/=+$/, "") };
}

test("packaged SSH connection test gates credentials on visible host trust and detects rotation", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-trust-"));
  const auth = keyPair();
  const authPath = path.join(root, "client-key"); writeFileSync(authPath, auth.privateKey, { mode: 0o600 });
  const profile = path.join(root, "profile");
  const connections = new Set<Connection>();
  const errors: Error[] = [];
  let attempts = 0;
  let stderr = "";
  let server: Server | undefined;
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  const listen = async (host: ReturnType<typeof keyPair>, port: number) => {
    server = new Server({ hostKeys: [host.privateKey] }, connection => {
      connections.add(connection);
      connection.on("close", () => connections.delete(connection));
      connection.on("error", () => {});
      connection.on("authentication", context => {
        if (context.method !== "none") attempts++;
        if (context.method !== "publickey" || !context.key.data.equals(auth.key.getPublicSSH())) { context.reject(); return; }
        if (context.signature && (!context.blob || auth.key.verify(context.blob, context.signature, context.hashAlgo) !== true)) { context.reject(); return; }
        context.accept();
      });
      connection.on("ready", () => connection.on("session", accept => {
        const session = accept();
        session.once("exec", (accept, reject, info) => {
          if (info.command !== "echo ok") { reject(); return; }
          const channel = accept(); channel.write("ok\n"); channel.exit(0); channel.end();
        });
      }));
    });
    server.on("error", (error: Error) => errors.push(error));
    server.listen(port, "127.0.0.1"); await once(server, "listening");
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("No SSH test address");
    return address.port;
  };
  const closeServer = async () => {
    // ssh2 lacks net.Server.listening at runtime; a guard would leak the port.
    connections.forEach(connection => connection.end());
    if (server) await new Promise<void>((resolve, reject) => server!.close(error => error && (error as NodeJS.ErrnoException).code !== "ERR_SERVER_NOT_RUNNING" ? reject(error) : resolve()));
  };
  try {
    const host = keyPair(); const port = await listen(host, 0);
    const source = path.join(root, "source"); mkdirSync(source);
    const env = { ...process.env, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
    writeFileSync(env.GIT_CONFIG_GLOBAL, "");
    execFileSync("git", ["init", "--initial-branch=main"], { cwd: source, env });
    execFileSync("git", ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "SSH trust navigation fixture"], { cwd: source, env });
    application = await electron.launch({ executablePath: packagedExecutable(), env, args: [`--user-data-dir=${profile}`] });
    application.process().stderr?.on("data", data => { stderr = (stderr + data.toString()).slice(-48_000); });
    const paths = await application.evaluate(({ app }) => ({ userData: app.getPath("userData"), userDataSwitch: app.commandLine.getSwitchValue("user-data-dir") }));
    expect(realpathSync(paths.userData)).toBe(realpathSync(profile));
    writeFileSync(test.info().outputPath("application-profile.json"), JSON.stringify(paths, null, 2));
    await test.info().attach("application-profile", { body: JSON.stringify(paths), contentType: "application/json" });
    const page = await application.firstWindow();
    page.on("pageerror", error => { stderr += `\nRenderer error: ${error.stack}`; });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "ssh-trust-fixture", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "SSH Connections", exact: true }).click();
    await page.getByRole("button", { name: /^(Add your first SSH connection|Add Connection)$/ }).click();
    const form = page.getByRole("dialog", { name: "Add SSH Connection", exact: true });
    await form.getByRole("textbox", { name: /^Label/ }).fill("Local verification fixture");
    await form.getByRole("textbox", { name: /^Host/ }).fill("127.0.0.1");
    await form.getByRole("spinbutton", { name: "Port", exact: true }).fill(String(port));
    await form.getByRole("textbox", { name: /^Username/ }).fill("crab-test");
    await form.getByRole("radio", { name: "Key File", exact: true }).check();
    await form.getByRole("textbox", { name: "Key file path", exact: true }).fill(authPath);
    const start = form.getByRole("button", { name: "Test SSH connection", exact: true });
    await start.click();
    const trust = page.getByRole("dialog", { name: "Verify Host Key", exact: true });
    await expect(trust.getByText(host.fingerprint, { exact: true })).toBeVisible();
    await expect(trust.getByRole("button", { name: "Reject", exact: true })).toBeFocused();
    await page.keyboard.press("Tab"); await expect(trust.getByRole("button", { name: "Accept", exact: true })).toBeFocused();
    await page.keyboard.press("Tab"); await expect(trust.getByRole("button", { name: "Close", exact: true })).toBeFocused();
    await page.screenshot({ path: test.info().outputPath("unknown-host-trust.png") });
    expect(attempts).toBe(0);
    await trust.getByRole("button", { name: "Reject", exact: true }).click();
    await expect(form.getByRole("alert")).toContainText(/verification failed/i);
    expect(attempts).toBe(0);
    await start.click();
    await expect(trust).toBeVisible();
    // Test Connection uses a five-second network deadline. Reviewing the
    // actual dialog beyond it must neither authenticate nor expire that clock.
    await page.waitForTimeout(5200);
    expect(attempts).toBe(0);
    await trust.getByRole("button", { name: "Accept", exact: true }).click();
    await expect(form.getByRole("alert")).toContainText("Connection successful");
    expect(attempts).toBeGreaterThan(0);
    const pins = JSON.parse(readFileSync(path.join(profile, "ssh-known-hosts.json"), "utf8")) as Array<{ fingerprint: string; port: number }>;
    expect(pins).toMatchObject([{ fingerprint: host.fingerprint, port }]);
    const prior = attempts;
    await start.click();
    await expect.poll(() => attempts).toBeGreaterThan(prior);
    await expect(form.getByRole("alert")).toContainText("Connection successful");
    await expect(trust).toHaveCount(0);
    await closeServer(); attempts = 0;
    const rotated = keyPair(); await listen(rotated, port);
    await start.click();
    const changed = page.getByRole("alertdialog", { name: /Host Key Changed/ });
    await expect(changed.getByText(host.fingerprint, { exact: true })).toBeVisible();
    await expect(changed.getByText(rotated.fingerprint, { exact: true })).toBeVisible();
    await expect(changed.getByRole("button", { name: /Accept new key/ })).toBeDisabled();
    await page.screenshot({ path: test.info().outputPath("changed-host-key.png") });
    await changed.getByRole("button", { name: "Reject", exact: true }).click();
    await expect(form.getByRole("alert")).toContainText(/verification failed/i);
    expect(attempts).toBe(0);
    expect(JSON.parse(readFileSync(path.join(profile, "ssh-known-hosts.json"), "utf8"))).toMatchObject([{ fingerprint: host.fingerprint }]);
    await start.click();
    await expect(changed).toBeVisible();
    await changed.getByRole("checkbox").check();
    await changed.getByRole("button", { name: /Accept new key/ }).click();
    await expect(form.getByRole("alert")).toContainText("Connection successful");
    expect(JSON.parse(readFileSync(path.join(profile, "ssh-known-hosts.json"), "utf8"))).toMatchObject([{ fingerprint: rotated.fingerprint }]);
    await page.screenshot({ path: test.info().outputPath("verified-rotated-connection.png") });
    expect(errors).toEqual([]);
  } catch (error) {
    await test.info().attach("original-failure", { body: error instanceof Error ? error.stack ?? error.message : String(error), contentType: "text/plain" });
    if (application) {
      const page = application.windows()[0];
      if (page && !page.isClosed()) await page.screenshot({ path: test.info().outputPath("failure-state.png") }).catch(() => {});
    }
    throw error;
  } finally {
    let cleanupFailure: unknown;
    if (application) {
      let guard: ReturnType<typeof setTimeout> | undefined;
      try { await Promise.race([application.close(), new Promise<never>((_, reject) => { guard = setTimeout(() => { application!.process().kill("SIGKILL"); reject(new Error("Packaged application did not close cleanly")); }, 15_000); })]); }
      catch (error) { cleanupFailure = error; }
      finally { clearTimeout(guard); }
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    await test.info().attach("application-stderr", { body: stderr, contentType: "text/plain" });
    await closeServer(); rmSync(root, { recursive: true, force: true });
    if (cleanupFailure) throw cleanupFailure;
  }
});
