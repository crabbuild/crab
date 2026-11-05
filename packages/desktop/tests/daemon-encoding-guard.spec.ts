/**
 * Source-Level Encoding Guard Test
 *
 * Verifies that no base64 encoding or toString('utf8') calls exist in the
 * PTY daemon data path. The daemon uses binary payload tails in frames —
 * any base64 encoding on the data path would be a ~33% wire overhead
 * regression plus unnecessary encode/decode passes on every chunk.
 */

import { describe, it, expect } from "vitest";
import { execSync } from "node:child_process";
import * as path from "node:path";
import * as fs from "node:fs";

const DAEMON_DIR = path.resolve(__dirname, "../electron/daemon");
const GUARD_SCRIPT = path.join(DAEMON_DIR, "encoding-guard.sh");

describe("daemon encoding guard", () => {
  it("encoding-guard.sh exists and is executable", () => {
    expect(fs.existsSync(GUARD_SCRIPT)).toBe(true);
    if (process.platform !== "win32") {
      expect(fs.statSync(GUARD_SCRIPT).mode & 0o100).not.toBe(0);
    }
  });

  it("no base64 or toString('utf8') violations in data-path files", () => {
    const result = execSync(`bash "${GUARD_SCRIPT}"`, {
      encoding: "utf-8",
      cwd: DAEMON_DIR,
    });
    expect(result).toContain("OK: No encoding violations");
  });

  it("data-path files do not use toString('base64')", () => {
    const dataPathFiles = [
      "protocol.ts",
      "daemon-server.ts",
      "stream-data-batcher.ts",
      "daemon-client.ts",
    ];

    for (const filename of dataPathFiles) {
      const filePath = path.join(DAEMON_DIR, filename);
      if (!fs.existsSync(filePath)) continue;
      const content = fs.readFileSync(filePath, "utf-8");
      const matches = content.match(/\.toString\(['"]base64['"]\)/g);
      expect(
        matches,
        `${filename} should not contain toString('base64')`,
      ).toBeNull();
    }
  });

  it("data-path files do not use Buffer.from(*, 'base64')", () => {
    const dataPathFiles = [
      "protocol.ts",
      "daemon-server.ts",
      "stream-data-batcher.ts",
      "daemon-client.ts",
    ];

    for (const filename of dataPathFiles) {
      const filePath = path.join(DAEMON_DIR, filename);
      if (!fs.existsSync(filePath)) continue;
      const content = fs.readFileSync(filePath, "utf-8");
      const matches = content.match(/Buffer\.from\([^)]*,\s*['"]base64['"]\)/g);
      expect(
        matches,
        `${filename} should not contain Buffer.from(*, 'base64')`,
      ).toBeNull();
    }
  });

  it("data-path files do not use toString('utf8') in frame path", () => {
    const dataPathFiles = [
      "protocol.ts",
      "daemon-server.ts",
      "stream-data-batcher.ts",
      "daemon-client.ts",
    ];

    for (const filename of dataPathFiles) {
      const filePath = path.join(DAEMON_DIR, filename);
      if (!fs.existsSync(filePath)) continue;
      const content = fs.readFileSync(filePath, "utf-8");
      const matches = content.match(/\.toString\(['"]utf8['"]\)/g);
      expect(
        matches,
        `${filename} should not contain toString('utf8')`,
      ).toBeNull();
    }
  });

  it("fd-handoff.ts is allowed to use base64 (snapshot serialization)", () => {
    const filePath = path.join(DAEMON_DIR, "fd-handoff.ts");
    if (!fs.existsSync(filePath)) return;
    const content = fs.readFileSync(filePath, "utf-8");
    // fd-handoff legitimately uses base64 for snapshot serialization
    // This test documents the exception — it should NOT be flagged
    const hasBase64 =
      /\.toString\(['"]base64['"]\)/.test(content) ||
      /Buffer\.from\([^)]*,\s*['"]base64['"]\)/.test(content);
    expect(hasBase64).toBe(true);
  });
});
