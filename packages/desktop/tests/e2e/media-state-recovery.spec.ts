import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { closeSync, existsSync, ftruncateSync, mkdtempSync, mkdirSync, openSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

for (const kind of ["audio", "video"] as const) for (const mode of ["paused", "playing", "blocked resume"] as const) test(`${kind} ${mode} media restores position and controls after native recovery`, async () => {
  const playing = mode !== "paused";
  test.skip(process.platform === "win32", "Native process fault injection uses POSIX enumeration");
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-media-state-"));
  const source = path.join(root, "source"); mkdirSync(source);
  const filename = kind === "audio" ? "recording.wav" : "recording.webm";
  const metadata = path.join(source, ".git"), offline = path.join(source, ".git-offline"), file = path.join(source, filename);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  const size = 44 + 60 * 192000, header = Buffer.alloc(44);
  header.write("RIFF"); header.writeUInt32LE(size - 8, 4); header.write("WAVEfmt ", 8);
  header.writeUInt32LE(16, 16); header.writeUInt16LE(1, 20); header.writeUInt16LE(2, 22);
  header.writeUInt32LE(48000, 24); header.writeUInt32LE(192000, 28);
  header.writeUInt16LE(4, 32); header.writeUInt16LE(16, 34); header.write("data", 36); header.writeUInt32LE(size - 44, 40);
  if (kind === "audio") {
    writeFileSync(file, header); const descriptor = openSync(file, "r+");
    try { ftruncateSync(descriptor, size); } finally { closeSync(descriptor); }
  } else writeFileSync(file, readFileSync(path.resolve(__dirname, "../fixtures/media/seek.webm")));
  const duration = kind === "audio" ? 60 : 4, position = kind === "audio" ? 20 : 1;
  git(["add", "."]); git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Media state fixture"]);
  const index = readFileSync(path.join(metadata, "index"));
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "media-state", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
    const media = page.locator(kind);
    await expect.poll(() => media.evaluate((node: HTMLMediaElement) => node.duration)).toBe(duration);
    const oldUrl = await media.getAttribute("src");
    await media.evaluate((node: HTMLMediaElement, position) => { node.currentTime = position; node.volume = 0.35; node.muted = true; node.playbackRate = 1.25; }, position);
    await expect.poll(() => media.evaluate((node: HTMLMediaElement, position) => !node.seeking && Math.abs(node.currentTime - position) < 0.1, position)).toBe(true);
    if (playing) {
      await media.evaluate((node: HTMLMediaElement) => node.play());
      await expect.poll(() => media.evaluate((node: HTMLMediaElement, position) => node.currentTime > position + 0.1, position)).toBe(true);
    }
    const before = await media.evaluate((node: HTMLMediaElement) => ({ position: node.currentTime, volume: node.volume, muted: node.muted, rate: node.playbackRate, paused: node.paused }));
    const previousPlayer = await media.elementHandle();
    if (!previousPlayer) throw new Error("The active media element is missing");
    if (mode === "blocked resume") await page.evaluate(oldUrl => {
      const play = HTMLMediaElement.prototype.play;
      let blocked = false;
      HTMLMediaElement.prototype.play = function () {
        if (!blocked && this.getAttribute("src") !== oldUrl) { blocked = true; return Promise.reject(new DOMException("Fixture autoplay denied", "NotAllowedError")); }
        return play.call(this);
      };
    }, oldUrl);
    renameSync(metadata, offline);
    const row = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).find(row => row && Number(row[2]) === application!.process().pid && row[3].includes("/agent/crab-desktop-agent "));
    if (!row) throw new Error("Owned native agent not found");
    process.kill(Number(row[1]), "SIGKILL");
    await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
    await expect(page.getByRole("status").filter({ hasText: "Waiting for repository recovery" })).toBeVisible();
    const detached = await previousPlayer.evaluate((node: HTMLMediaElement) => ({ paused: node.paused, src: node.getAttribute("src") }));
    writeFileSync(test.info().outputPath("detached-player-observed.json"), JSON.stringify(detached, null, 2));
    expect(detached).toEqual({ paused: true, src: null });
    renameSync(offline, metadata);
    await page.getByRole("button", { name: "Retry reopening repository", exact: true }).click();
    await expect(media).toBeVisible();
    await expect.poll(() => media.evaluate((node: HTMLMediaElement) => node.duration)).toBe(duration);
    const observed = await media.evaluate((node: HTMLMediaElement) => ({ position: node.currentTime, volume: node.volume, muted: node.muted, rate: node.playbackRate, paused: node.paused }));
    writeFileSync(test.info().outputPath("media-state-observed.json"), JSON.stringify({ before, observed }, null, 2));
    await expect.poll(() => media.evaluate((node: HTMLMediaElement, position) => !node.seeking && node.currentTime >= position - 0.1, position)).toBe(true);
    if (mode === "blocked resume") {
      const hint = page.getByRole("status").filter({ hasText: "Press Play to resume" });
      await expect(hint).toBeVisible();
      expect(await media.evaluate((node: HTMLMediaElement) => node.paused)).toBe(true);
      await media.evaluate((node: HTMLMediaElement) => node.play());
      await expect(hint).toHaveCount(0);
    }
    const after = await media.evaluate((node: HTMLMediaElement) => ({ position: node.currentTime, volume: node.volume, muted: node.muted, rate: node.playbackRate, paused: node.paused }));
    expect(after).toMatchObject({ volume: before.volume, muted: before.muted, rate: before.rate, paused: !playing });
    if (!playing) expect(after.position).toBeCloseTo(before.position, 1);
    expect(await media.getAttribute("src")).not.toBe(oldUrl);
    expect(await page.evaluate(async url => (await fetch(url!, { method: "HEAD" })).status, oldUrl)).toBe(410);
    expect(readFileSync(path.join(metadata, "index")).equals(index)).toBe(true);
    await media.evaluate((node: HTMLMediaElement) => node.pause());
    await page.screenshot({ path: test.info().outputPath("media-state-recovered.png") });
    writeFileSync(test.info().outputPath("media-state-proof.json"), JSON.stringify({ kind, mode, before, after, oldReaderRevoked: true, indexUnchanged: true }, null, 2));
  } finally {
    if (existsSync(offline)) renameSync(offline, metadata);
    if (application) {
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
