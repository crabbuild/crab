import { createHash } from "node:crypto";
import * as path from "node:path";

/** Resolve a daemon control-file path to the platform's IPC address. */
export function socketAddress(socketPath: string): string {
  if (process.platform !== "win32") return socketPath;
  // Windows pipes have a flat namespace, independent of the filesystem.
  // Hash the full profile/protocol path to keep addresses bounded and isolated.
  const identity = path.resolve(socketPath).toLowerCase();
  const name = createHash("sha256").update(identity).digest("hex");
  return `\\\\.\\pipe\\crab-pty-${name}`;
}
