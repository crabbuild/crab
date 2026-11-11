import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import {
  resolveDaemonPath,
  resolveDaemonEntryPath,
  type ResolveDaemonPathOptions,
} from "../electron/daemon/resolve-daemon-path";

// These unit fixtures model logical package paths as ordinary directories.
// Packaged smoke tests qualify Electron's actual ASAR loader separately.
const runtime = process as { noAsar?: boolean };
let previousNoAsar: boolean | undefined;
beforeEach(() => { previousNoAsar = runtime.noAsar; runtime.noAsar = true; });
afterEach(() => { if (previousNoAsar === undefined) delete runtime.noAsar; else runtime.noAsar = previousNoAsar; });

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-path-test-"));
}

function createFile(filePath: string): void {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, "// daemon entry stub");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("resolveDaemonPath", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = createTmpDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  describe("env var override", () => {
    it("returns the env override path when file exists", () => {
      const envPath = path.join(tmpDir, "custom", "daemon-main.cjs");
      createFile(envPath);

      const result = resolveDaemonPath({
        isDev: true,
        resourcesPath: "/fake/resources",
        electronDistDir: "/fake/dist",
        envOverride: envPath,
      });

      expect(result.entryPath).toBe(envPath);
      expect(result.fromEnv).toBe(true);
    });

    it("throws when env override path does not exist", () => {
      const envPath = path.join(tmpDir, "nonexistent", "daemon-main.cjs");

      expect(() =>
        resolveDaemonPath({
          isDev: true,
          resourcesPath: "/fake/resources",
          electronDistDir: "/fake/dist",
          envOverride: envPath,
        }),
      ).toThrow(/CRAB_DAEMON_ENTRY is set/);
    });

    it("includes the path in the error message", () => {
      const envPath = "/some/bad/path.js";

      expect(() =>
        resolveDaemonPath({
          isDev: true,
          resourcesPath: "/fake/resources",
          electronDistDir: "/fake/dist",
          envOverride: envPath,
        }),
      ).toThrow(envPath);
    });
  });

  describe("development mode", () => {
    it("resolves from electronDistDir in dev mode", () => {
      const distDir = path.join(tmpDir, "dist-electron", "daemon");
      const entryPath = path.join(distDir, "daemon-main.cjs");
      createFile(entryPath);

      const result = resolveDaemonPath({
        isDev: true,
        resourcesPath: "/fake/resources",
        electronDistDir: distDir,
      });

      expect(result.entryPath).toBe(entryPath);
      expect(result.fromEnv).toBe(false);
      expect(result.isDev).toBe(true);
    });

    it("throws when dev candidate does not exist", () => {
      const distDir = path.join(tmpDir, "dist-electron", "daemon");

      expect(() =>
        resolveDaemonPath({
          isDev: true,
          resourcesPath: "/fake/resources",
          electronDistDir: distDir,
        }),
      ).toThrow(/development mode/);
    });

    it("does not check resources path in dev mode", () => {
      // Even if the resources path has the file, dev mode only checks distDir
      const resourcesPath = path.join(tmpDir, "resources");
      createFile(path.join(resourcesPath, "app.asar", "dist-electron", "daemon", "daemon-main.cjs"));

      const distDir = path.join(tmpDir, "dist-electron", "daemon");
      // distDir does NOT have the file

      expect(() =>
        resolveDaemonPath({
          isDev: true,
          resourcesPath,
          electronDistDir: distDir,
        }),
      ).toThrow(/development mode/);
    });
  });

  describe("production mode", () => {
    it("loads packaged JavaScript through ASAR even when an unpacked copy exists", () => {
      const resourcesPath = path.join(tmpDir, "resources");
      const daemon = path.join("dist-electron", "daemon", "daemon-main.cjs");
      const entryPath = path.join(resourcesPath, "app.asar", daemon);
      createFile(entryPath);
      createFile(path.join(resourcesPath, "app.asar.unpacked", daemon));

      const result = resolveDaemonPath({
        isDev: false,
        resourcesPath,
        electronDistDir: "/fake/dist",
      });

      expect(result.entryPath).toBe(entryPath);
    });

    it("resolves from resources/app.asar/dist-electron/daemon/ in prod mode", () => {
      const resourcesPath = path.join(tmpDir, "resources");
      const entryPath = path.join(resourcesPath, "app.asar", "dist-electron", "daemon", "daemon-main.cjs");
      createFile(entryPath);

      const result = resolveDaemonPath({
        isDev: false,
        resourcesPath,
        electronDistDir: "/fake/dist",
      });

      expect(result.entryPath).toBe(entryPath);
      expect(result.fromEnv).toBe(false);
      expect(result.isDev).toBe(false);
    });

    it("throws when prod candidate does not exist", () => {
      const resourcesPath = path.join(tmpDir, "resources");

      expect(() =>
        resolveDaemonPath({
          isDev: false,
          resourcesPath,
          electronDistDir: "/fake/dist",
        }),
      ).toThrow(/production mode/);
    });

    it("does not check dist dir in prod mode", () => {
      // Even if distDir has the file, prod mode only checks resources
      const distDir = path.join(tmpDir, "dist-electron", "daemon");
      createFile(path.join(distDir, "daemon-main.cjs"));

      const resourcesPath = path.join(tmpDir, "resources");
      // resources does NOT have the file

      expect(() =>
        resolveDaemonPath({
          isDev: false,
          resourcesPath,
          electronDistDir: distDir,
        }),
      ).toThrow(/production mode/);
    });
  });

  describe("error messages", () => {
    it("includes searched paths in the error", () => {
      const distDir = path.join(tmpDir, "dist");

      try {
        resolveDaemonPath({
          isDev: true,
          resourcesPath: "/fake",
          electronDistDir: distDir,
        });
        expect.fail("should have thrown");
      } catch (err) {
        const msg = (err as Error).message;
        expect(msg).toContain(path.join(distDir, "daemon-main.cjs"));
        expect(msg).toContain("Run the build step");
      }
    });

    it("suggests CRAB_DAEMON_ENTRY override in error", () => {
      expect(() =>
        resolveDaemonPath({
          isDev: true,
          resourcesPath: "/fake",
          electronDistDir: "/nonexistent",
        }),
      ).toThrow(/CRAB_DAEMON_ENTRY/);
    });
  });
});

describe("resolveDaemonEntryPath", () => {
  let tmpDir: string;
  const originalEnv = process.env.CRAB_DAEMON_ENTRY;

  beforeEach(() => {
    tmpDir = createTmpDir();
    delete process.env.CRAB_DAEMON_ENTRY;
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
    if (originalEnv !== undefined) {
      process.env.CRAB_DAEMON_ENTRY = originalEnv;
    } else {
      delete process.env.CRAB_DAEMON_ENTRY;
    }
  });

  it("resolves dev path when isPackaged is false", () => {
    const distDir = path.join(tmpDir, "dist-electron", "daemon");
    const entryPath = path.join(distDir, "daemon-main.cjs");
    createFile(entryPath);

    const result = resolveDaemonEntryPath(false, "/fake/resources", distDir);

    expect(result.entryPath).toBe(entryPath);
    expect(result.isDev).toBe(true);
    expect(result.fromEnv).toBe(false);
  });

  it("resolves prod path when isPackaged is true", () => {
    const resourcesPath = path.join(tmpDir, "resources");
    const entryPath = path.join(resourcesPath, "app.asar", "dist-electron", "daemon", "daemon-main.cjs");
    createFile(entryPath);

    const result = resolveDaemonEntryPath(true, resourcesPath, "/fake/dist");

    expect(result.entryPath).toBe(entryPath);
    expect(result.isDev).toBe(false);
    expect(result.fromEnv).toBe(false);
  });

  it("uses CRAB_DAEMON_ENTRY env var when set", () => {
    const customPath = path.join(tmpDir, "custom-daemon.js");
    createFile(customPath);
    process.env.CRAB_DAEMON_ENTRY = customPath;

    const result = resolveDaemonEntryPath(false, "/fake/resources", "/fake/dist");

    expect(result.entryPath).toBe(customPath);
    expect(result.fromEnv).toBe(true);
  });

  it("ignores empty CRAB_DAEMON_ENTRY", () => {
    process.env.CRAB_DAEMON_ENTRY = "";
    const distDir = path.join(tmpDir, "dist-electron", "daemon");
    const entryPath = path.join(distDir, "daemon-main.cjs");
    createFile(entryPath);

    const result = resolveDaemonEntryPath(false, "/fake/resources", distDir);

    expect(result.entryPath).toBe(entryPath);
    expect(result.fromEnv).toBe(false);
  });
});
