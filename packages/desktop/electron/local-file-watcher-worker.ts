import { opendir, lstat } from "node:fs/promises";
import * as path from "node:path";
import { createHash } from "node:crypto";
import { parentPort, workerData } from "node:worker_threads";

const IGNORED = new Set(["node_modules", "target", "__pycache__", ".venv", ".venv-crab-py", "dist", "build", ".next", ".turbo"]);
const GIT_INTERESTING = new Set(["index", "HEAD", "MERGE_HEAD", "REBASE_HEAD", "CHERRY_PICK_HEAD", "FETCH_HEAD", "ORIG_HEAD", "COMMIT_EDITMSG"]);

function ignored(relative: string): boolean {
  const parts = relative.split("/");
  if (parts.some(part => IGNORED.has(part))) return true;
  for (let i = 0; i < parts.length - 1; i++) {
    if (parts[i] === ".crab" && ["staging", "cache", "slatedb"].includes(parts[i + 1])) return true;
    if (parts[i] === ".git" && parts[i + 1] !== "refs" && !GIT_INTERESTING.has(parts[i + 1])) return true;
  }
  return false;
}

const port = parentPort;
if (!port) throw new Error("The filesystem monitor requires a worker owner.");
const root = String(workerData);
let stopped = false, scanning = false;
let timer: ReturnType<typeof setTimeout> | undefined;
let previous: string | undefined;
let unavailable = false;

function missing(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}

async function fingerprint(): Promise<string> {
  const total = Buffer.alloc(32);
  const add = (record: string) => {
    const digest = createHash("sha256").update(record).digest();
    for (let i = 0; i < total.length; i++) total[i] ^= digest[i];
  };
  async function walk(directory: string, prefix: string): Promise<void> {
    let entries;
    try { entries = await opendir(directory); }
    catch (error) {
      if (prefix && missing(error)) return;
      // An inaccessible subtree still contributes its directory metadata.
      // Its mode change is observable when access is repaired.
      if (prefix && error instanceof Error && "code" in error && error.code === "EACCES") return;
      throw error;
    }
    for await (const entry of entries) {
      if (stopped) return;
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (ignored(relative)) continue;
      const full = path.join(directory, entry.name);
      let info;
      try { info = await lstat(full, { bigint: true }); }
      catch (error) { if (missing(error)) continue; throw error; }
      add(`${relative}\0${info.mode}:${info.ino}:${info.size}:${info.mtimeNs}:${info.ctimeNs}`);
      if (info.isDirectory()) await walk(full, relative);
    }
  }
  await walk(root, "");
  return total.toString("hex");
}

async function scan(): Promise<void> {
  scanning = true;
  try {
    // Native directory event streams can silently miss changes and synchronously
    // block during close. Metadata snapshots keep one portable invalidation path
    // without reading large-file contents or queueing overlapping scans.
    const current = await fingerprint();
    if (stopped) return;
    if (previous === undefined) port!.postMessage("ready");
    // Views may have read while the first inventory was being gathered.
    if (current !== previous || unavailable) port!.postMessage("changed");
    previous = current;
    unavailable = false;
  } catch (error) {
    if (previous === undefined) { stopped = true; throw error; }
    // Invalidate on loss and repair, including repair to identical metadata.
    // Consumers reread their source and show its error instead of stale success.
    if (!unavailable) port!.postMessage("changed");
    unavailable = true;
  }
  finally {
    scanning = false;
    if (stopped) port!.close();
    else timer = setTimeout(() => { void scan(); }, 1000);
  }
}

port.on("message", () => {
  stopped = true;
  clearTimeout(timer);
  if (!scanning) port.close();
});
void scan();
