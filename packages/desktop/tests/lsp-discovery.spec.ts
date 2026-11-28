import { expect, it, vi } from "vitest";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

vi.mock("electron", () => ({ app: { getAppPath: () => process.cwd() } }));

function fixture() {
  const root = mkdtempSync(path.join(tmpdir(), "crab-lsp-discovery-"));
  const bin = path.join(root, "bin with spaces");
  const release = path.join(root, "release");
  const entry = path.join(root, "version.cjs");
  mkdirSync(bin);
  const quote = (value: string) => "'" + value.replaceAll("'", "'\\''") + "'";
  writeFileSync(entry, `
    const fs = require('node:fs');
    if (!process.argv.includes('--version')) {
      fs.writeFileSync(${JSON.stringify(path.join(root, "server-started"))}, 'started');
      process.exit();
    }
    const pidFile = ${JSON.stringify(path.join(root, "probe.pid"))};
    fs.writeFileSync(pidFile + '.tmp', String(process.pid));
    fs.renameSync(pidFile + '.tmp', pidFile);
    process.stdin.resume();
    process.stdin.once('end', () => {
      const timer = setInterval(() => {
        if (!fs.existsSync(${JSON.stringify(release)})) return;
        clearInterval(timer);
        process.stdout.write('fixture 1.0\\n');
      }, 10);
    });
    setTimeout(() => process.exit(), 6_000).unref();
  `);
  writeFileSync(path.join(bin, "rust-analyzer"), `#!/bin/sh\nELECTRON_RUN_AS_NODE=1 exec ${quote(process.execPath)} ${quote(entry)} "$@"\n`, { mode: 0o700 });
  vi.stubEnv("PATH", bin + path.delimiter + (process.env.PATH ?? ""));
  vi.stubEnv("HOME", root);
  vi.resetModules();
  return { root, release, cleanup: () => { vi.unstubAllEnvs(); rmSync(root, { recursive: true, force: true }); } };
}

it.skipIf(process.platform === "win32")("keeps the event loop available while a server reports its version", async () => {
  const { release, cleanup } = fixture();
  let heartbeat: ReturnType<typeof setTimeout> | undefined;
  const { discoverServers, stopDiscovery } = await import("../electron/lsp-discovery");
  try {
    heartbeat = setTimeout(() => writeFileSync(release, "ready"), 50);
    const servers = await discoverServers();
    expect(servers.find(server => server.language === "rust")?.version).toBe("fixture 1.0");
  } finally {
    clearTimeout(heartbeat);
    if (!existsSync(release)) writeFileSync(release, "ready");
    await stopDiscovery();
    cleanup();
  }
}, 20_000);

it.skipIf(process.platform === "win32")("shutdown drains version children from a scan replaced by refresh", async () => {
  const { root, cleanup } = fixture();
  const { discoverServers, refresh, stopDiscovery } = await import("../electron/lsp-discovery");
  const pidFile = path.join(root, "probe.pid");
  const first = discoverServers().catch(() => null);
  try {
    await expect.poll(() => existsSync(pidFile)).toBe(true);
    const firstPid = Number(readFileSync(pidFile, "utf8"));
    const second = refresh().catch(() => null);
    await expect.poll(() => Number(readFileSync(pidFile, "utf8"))).not.toBe(firstPid);
    const secondPid = Number(readFileSync(pidFile, "utf8"));
    await stopDiscovery();
    const alive = [firstPid, secondPid].filter(pid => {
      try { process.kill(pid, 0); return true; } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
        return false;
      }
    });
    expect({ results: await Promise.all([first, second]), alive }).toEqual({ results: [null, null], alive: [] });
  } finally {
    await stopDiscovery();
    await first;
    cleanup();
  }
}, 20_000);

it.skipIf(process.platform === "win32").each(["language", "all"])("stopping %s during discovery prevents a late server and drains the version child", async scope => {
  const { root, release, cleanup } = fixture();
  const { LspManager } = await import("../electron/lsp-manager");
  const manager = new LspManager();
  const started = manager.start("rust", root).then(() => false, () => true);
  try {
    await expect.poll(() => existsSync(path.join(root, "probe.pid"))).toBe(true);
    const pid = Number(readFileSync(path.join(root, "probe.pid"), "utf8"));
    if (scope === "all") await manager.stopAll();
    else await manager.stop("rust");
    writeFileSync(release, "ready");
    const cancelled = await started;
    let alive = true;
    try { process.kill(pid, 0); } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
      alive = false;
    }
    expect({ cancelled, alive, spawned: existsSync(path.join(root, "server-started")) }).toEqual({ cancelled: true, alive: false, spawned: false });
  } finally {
    await manager.stopAll();
    await started;
    cleanup();
  }
}, 20_000);
