import { promises as fs } from "node:fs";
import type { FileHandle } from "node:fs/promises";
import * as path from "node:path";
import type { FsReadResult } from "./repo-provider";

/** Read one local file with a bounded allocation, independently of native services. */
export async function readTextFile(fullPath: string, maxBytes = 512 * 1024): Promise<FsReadResult> {
  if (!path.isAbsolute(fullPath)) throw new Error("The read target must be an absolute local filesystem path.");
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 0 || maxBytes > 4 * 1024 * 1024) throw new Error("Invalid text read limit.");
  let file: FileHandle;
  try { file = await fs.open(fullPath, "r"); }
  catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") return { exists: false, content: null };
    throw error;
  }
  try {
    const stat = await file.stat();
    if (stat.isDirectory()) return { exists: true, content: null };
    if (!stat.isFile()) throw new Error("The read target is not a regular file.");
    if (stat.size > maxBytes) throw new Error(`The file exceeds the ${maxBytes}-byte text read limit.`);
    // The file may grow after stat. Reading one extra byte keeps allocation
    // bounded and distinguishes a complete document from a truncated prefix.
    const bytes = Buffer.alloc(maxBytes + 1);
    let length = 0;
    while (length < bytes.length) {
      const { bytesRead } = await file.read(bytes, length, bytes.length - length, null);
      if (!bytesRead) break;
      length += bytesRead;
    }
    if (length > maxBytes) throw new Error(`The file exceeds the ${maxBytes}-byte text read limit.`);
    return { exists: true, content: new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(0, length)) };
  } finally { await file.close(); }
}
