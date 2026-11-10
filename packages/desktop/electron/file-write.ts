import * as fs from "node:fs";
import * as path from "node:path";
import { randomUUID } from "node:crypto";

const active = new Set<string>();
const pending = new Set<Promise<number>>();
let stopping = false;
const openings = new Map<Promise<fs.promises.FileHandle>, string>();
const snapshots = new Set<{ root: string; paths: Set<string>; openings: Set<Promise<fs.promises.FileHandle>> }>();

/** Capture exact owned temporary paths overlapping a metadata read. */
export function captureFilePublications(root: string) {
  const snapshot = { root, paths: new Set([...active].filter(file => within(root, file))),
    openings: new Set([...openings].filter(([, file]) => within(root, file)).map(([opening]) => opening)) };
  snapshots.add(snapshot);
  return {
    async finish(): Promise<Set<string>> {
      try {
        // An exclusive open can be visible to Git before its promise resolves.
        // Failed acquisition never becomes ownership of a pre-existing file.
        await Promise.allSettled([...snapshot.openings]);
        return snapshot.paths;
      } finally { snapshots.delete(snapshot); }
    },
  };
}

function within(root: string, file: string): boolean {
  const relative = path.relative(root, file);
  return relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative);
}

function missing(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}

async function destination(fullPath: string): Promise<string> {
  try { return await fs.promises.realpath(fullPath); }
  catch (error) {
    if (!missing(error)) throw error;
    // Preserve existing links, including dangling links: a failed resolution
    // must not turn an editor save into replacement of the link itself.
    try { await fs.promises.lstat(fullPath); }
    catch (metadataError) {
      if (!missing(metadataError)) throw metadataError;
      const parent = path.dirname(fullPath);
      await fs.promises.mkdir(parent, { recursive: true });
      return path.join(await fs.promises.realpath(parent), path.basename(fullPath));
    }
    throw error;
  }
}

/** Reject new saves and join admitted publications, including failed cleanup. */
export async function stopFileWrites(): Promise<void> {
  stopping = true;
  // The original caller owns success/failure. Shutdown still joins rejection,
  // which is delivered only after the writer has completed its cleanup.
  await Promise.allSettled([...pending]);
}

/** Publish complete text without mutating an existing source before commit. */
export function writeTextAtomically(fullPath: string, content: string): Promise<number> {
  if (stopping) return Promise.reject(Object.assign(new Error("Local file services are shutting down."), { code: "ESHUTDOWN" }));
  const work = publishText(fullPath, content);
  pending.add(work);
  void work.then(() => pending.delete(work), () => pending.delete(work));
  return work;
}

async function publishText(fullPath: string, content: string): Promise<number> {
  if (!path.isAbsolute(fullPath)) throw Object.assign(new Error("The write target must be an absolute local filesystem path."), { code: "EINVAL" });
  const target = await destination(fullPath);
  let previous: fs.Stats | undefined;
  try { previous = await fs.promises.stat(target); }
  catch (error) { if (!missing(error)) throw error; }
  if (previous) {
    if (!previous.isFile()) throw Object.assign(new Error("The write target is not a regular file."), { code: "EINVAL" });
    if (!(previous.mode & 0o222)) throw Object.assign(new Error("The write target is read-only."), { code: "EACCES" });
    // Rename checks the directory. Probe the target without truncation too,
    // so atomic publication cannot bypass its write permission or ACL.
    const access = await fs.promises.open(target, fs.constants.O_WRONLY);
    await access.close();
  }
  const temporary = path.join(path.dirname(target), `.crab-write-${randomUUID()}`);
  // Exclusive open establishes cleanup ownership. An existing entry is never
  // removed on failure to acquire it; callers get the original open error.
  const opening = fs.promises.open(temporary, "wx", previous ? 0o600 : 0o666);
  openings.set(opening, temporary);
  for (const snapshot of snapshots) if (within(snapshot.root, temporary)) snapshot.openings.add(opening);
  let file: fs.promises.FileHandle;
  try { file = await opening; }
  finally { openings.delete(opening); }
  active.add(temporary);
  for (const snapshot of snapshots) if (within(snapshot.root, temporary)) snapshot.paths.add(temporary);
  try {
    const errors: unknown[] = [];
    try {
      await fs.promises.writeFile(file, content, "utf8");
      if (previous) await file.chmod(previous.mode & 0o7777);
      await file.sync();
    } catch (error) { errors.push(error); }
    try { await file.close(); }
    catch (error) { errors.push(error); }
    if (!errors.length) {
      try { await fs.promises.rename(temporary, target); }
      catch (error) { errors.push(error); }
    }
    if (errors.length) {
      try { await fs.promises.unlink(temporary); }
      catch (error) { if (!missing(error)) errors.push(error); }
      if (errors.length === 1) throw errors[0];
      const message = errors.map(error => error instanceof Error ? error.message : String(error)).join("; ");
      throw new AggregateError(errors, message, { cause: errors[0] });
    }
    return Buffer.byteLength(content, "utf8");
  } finally { active.delete(temporary); }
}
