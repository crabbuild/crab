import * as fs from "node:fs";
import * as path from "node:path";
import { randomUUID } from "node:crypto";

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

/** Publish complete text without mutating an existing source before commit. */
export async function writeTextAtomically(fullPath: string, content: string): Promise<number> {
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
  const file = await fs.promises.open(temporary, "wx", previous ? 0o600 : 0o666);
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
}
