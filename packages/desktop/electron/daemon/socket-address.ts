import { createHash } from "node:crypto";
import * as path from "node:path";

/** Resolve a daemon control-file path to the platform's IPC address. */
export function socketAddress(socketPath: string): string {
  const windows = process.platform === "win32";
  // v2.0.0-alpha.1 exposes normal Unix socket paths to existing daemons.
  // Longer paths cannot represent an endpoint without kernel truncation.
  const limit = process.platform === "darwin" ? 103 : 107;
  if (!windows && Buffer.byteLength(socketPath) <= limit) return socketPath;
  const resolved = path.resolve(socketPath);
  const identity = windows ? resolved.toLowerCase() : resolved;
  const name = createHash("sha256").update(identity).digest("hex");
  return windows ? `\\\\.\\pipe\\crab-pty-${name}` : `/tmp/crab-pty-${process.getuid!()}-${name}/s`;
}
