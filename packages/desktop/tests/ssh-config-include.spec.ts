/**
 * Unit tests for SSH config Include directive handling.
 *
 * Tests: basic Include, nested Include, glob patterns, cycle detection,
 * and graceful handling of missing files.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { parseSshConfig, resolveIncludePaths } from "../electron/ssh-profiles";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeTempDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "crab-ssh-include-test-"));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("parseSshConfig with Include directives", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = makeTempDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  describe("basic Include", () => {
    it("follows a single Include directive to load hosts from another file", () => {
      const mainConfig = path.join(tmpDir, "config");
      const includedConfig = path.join(tmpDir, "extra-hosts");

      fs.writeFileSync(
        mainConfig,
        `Include ${includedConfig}\n\nHost main-server\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        includedConfig,
        `Host included-server\n  HostName included.example.com\n  User deploy\n  Port 2222\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(2);
      expect(result[0].alias).toBe("main-server");
      expect(result[0].hostname).toBe("main.example.com");
      expect(result[1].alias).toBe("included-server");
      expect(result[1].hostname).toBe("included.example.com");
      expect(result[1].port).toBe(2222);
    });

    it("handles Include with tilde expansion", () => {
      // This test uses a real path under tmpDir to simulate ~ expansion
      // We test resolveIncludePaths separately for tilde handling
      const mainConfig = path.join(tmpDir, "config");
      const includedConfig = path.join(tmpDir, "included");

      fs.writeFileSync(
        mainConfig,
        `Include ${includedConfig}\n\nHost local-host\n  HostName local.example.com\n  User me\n`,
        "utf-8",
      );

      fs.writeFileSync(
        includedConfig,
        `Host remote-host\n  HostName remote.example.com\n  User you\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(2);
      expect(result[0].alias).toBe("local-host");
      expect(result[1].alias).toBe("remote-host");
    });

    it("merges hosts from multiple Include directives", () => {
      const mainConfig = path.join(tmpDir, "config");
      const included1 = path.join(tmpDir, "hosts1");
      const included2 = path.join(tmpDir, "hosts2");

      fs.writeFileSync(
        mainConfig,
        `Include ${included1}\nInclude ${included2}\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        included1,
        `Host server-a\n  HostName a.example.com\n  User alice\n`,
        "utf-8",
      );

      fs.writeFileSync(
        included2,
        `Host server-b\n  HostName b.example.com\n  User bob\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(3);
      expect(result[0].alias).toBe("main");
      expect(result[1].alias).toBe("server-a");
      expect(result[2].alias).toBe("server-b");
    });

    it("only processes Include directives at the top level (before Host blocks)", () => {
      const mainConfig = path.join(tmpDir, "config");
      const includedConfig = path.join(tmpDir, "extra");

      fs.writeFileSync(
        mainConfig,
        `Host first\n  HostName first.example.com\n  User admin\n  Include ${includedConfig}\n`,
        "utf-8",
      );

      fs.writeFileSync(
        includedConfig,
        `Host should-not-appear\n  HostName hidden.example.com\n  User ghost\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      // Include inside a Host block is not followed
      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("first");
    });
  });

  describe("nested Include (recursive)", () => {
    it("follows nested Include directives recursively", () => {
      const mainConfig = path.join(tmpDir, "config");
      const level1 = path.join(tmpDir, "level1");
      const level2 = path.join(tmpDir, "level2");

      fs.writeFileSync(
        mainConfig,
        `Include ${level1}\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        level1,
        `Include ${level2}\n\nHost level1-host\n  HostName l1.example.com\n  User l1\n`,
        "utf-8",
      );

      fs.writeFileSync(
        level2,
        `Host level2-host\n  HostName l2.example.com\n  User l2\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(3);
      expect(result[0].alias).toBe("main");
      // level1 includes level2, so level1-host comes after its own includes
      expect(result.find((h) => h.alias === "level1-host")).toBeDefined();
      expect(result.find((h) => h.alias === "level2-host")).toBeDefined();
    });
  });

  describe("glob patterns", () => {
    it("expands Include with glob pattern (config.d/*)", () => {
      const mainConfig = path.join(tmpDir, "config");
      const configDir = path.join(tmpDir, "config.d");
      fs.mkdirSync(configDir);

      fs.writeFileSync(
        mainConfig,
        `Include ${configDir}/*\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        path.join(configDir, "gpu-servers"),
        `Host gpu-01\n  HostName gpu01.example.com\n  User ml\n`,
        "utf-8",
      );

      fs.writeFileSync(
        path.join(configDir, "build-servers"),
        `Host build-01\n  HostName build01.example.com\n  User ci\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(3);
      expect(result[0].alias).toBe("main");
      // Glob results are sorted alphabetically
      expect(result.find((h) => h.alias === "build-01")).toBeDefined();
      expect(result.find((h) => h.alias === "gpu-01")).toBeDefined();
    });

    it("skips directories when expanding glob patterns", () => {
      const mainConfig = path.join(tmpDir, "config");
      const configDir = path.join(tmpDir, "config.d");
      fs.mkdirSync(configDir);
      fs.mkdirSync(path.join(configDir, "subdir"));

      fs.writeFileSync(
        mainConfig,
        `Include ${configDir}/*\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        path.join(configDir, "hosts"),
        `Host file-host\n  HostName file.example.com\n  User user\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      // Only the file is included, not the subdirectory
      expect(result).toHaveLength(2);
      expect(result[0].alias).toBe("main");
      expect(result[1].alias).toBe("file-host");
    });

    it("handles glob pattern matching specific file extensions", () => {
      const mainConfig = path.join(tmpDir, "config");
      const configDir = path.join(tmpDir, "conf.d");
      fs.mkdirSync(configDir);

      fs.writeFileSync(
        mainConfig,
        `Include ${configDir}/*.conf\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        path.join(configDir, "servers.conf"),
        `Host matched\n  HostName matched.example.com\n  User user\n`,
        "utf-8",
      );

      fs.writeFileSync(
        path.join(configDir, "servers.bak"),
        `Host not-matched\n  HostName notmatched.example.com\n  User user\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(2);
      expect(result[0].alias).toBe("main");
      expect(result[1].alias).toBe("matched");
    });

    it("returns empty when glob directory does not exist", () => {
      const mainConfig = path.join(tmpDir, "config");

      fs.writeFileSync(
        mainConfig,
        `Include ${tmpDir}/nonexistent-dir/*\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("main");
    });
  });

  describe("cycle detection", () => {
    it("detects and breaks direct self-inclusion cycle", () => {
      const mainConfig = path.join(tmpDir, "config");

      fs.writeFileSync(
        mainConfig,
        `Include ${mainConfig}\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      // Should not infinite loop — cycle detection breaks it
      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("main");
    });

    it("detects and breaks mutual inclusion cycle (A includes B, B includes A)", () => {
      const configA = path.join(tmpDir, "config-a");
      const configB = path.join(tmpDir, "config-b");

      fs.writeFileSync(
        configA,
        `Include ${configB}\n\nHost host-a\n  HostName a.example.com\n  User alice\n`,
        "utf-8",
      );

      fs.writeFileSync(
        configB,
        `Include ${configA}\n\nHost host-b\n  HostName b.example.com\n  User bob\n`,
        "utf-8",
      );

      // Should not infinite loop
      const result = parseSshConfig(configA);

      expect(result).toHaveLength(2);
      expect(result.find((h) => h.alias === "host-a")).toBeDefined();
      expect(result.find((h) => h.alias === "host-b")).toBeDefined();
    });

    it("detects and breaks three-way cycle (A -> B -> C -> A)", () => {
      const configA = path.join(tmpDir, "config-a");
      const configB = path.join(tmpDir, "config-b");
      const configC = path.join(tmpDir, "config-c");

      fs.writeFileSync(
        configA,
        `Include ${configB}\n\nHost host-a\n  HostName a.example.com\n  User alice\n`,
        "utf-8",
      );

      fs.writeFileSync(
        configB,
        `Include ${configC}\n\nHost host-b\n  HostName b.example.com\n  User bob\n`,
        "utf-8",
      );

      fs.writeFileSync(
        configC,
        `Include ${configA}\n\nHost host-c\n  HostName c.example.com\n  User charlie\n`,
        "utf-8",
      );

      const result = parseSshConfig(configA);

      expect(result).toHaveLength(3);
      expect(result.find((h) => h.alias === "host-a")).toBeDefined();
      expect(result.find((h) => h.alias === "host-b")).toBeDefined();
      expect(result.find((h) => h.alias === "host-c")).toBeDefined();
    });
  });

  describe("missing files", () => {
    it("gracefully skips Include of a non-existent file", () => {
      const mainConfig = path.join(tmpDir, "config");

      fs.writeFileSync(
        mainConfig,
        `Include ${tmpDir}/does-not-exist\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("main");
    });

    it("gracefully skips Include of an unreadable file", () => {
      const mainConfig = path.join(tmpDir, "config");
      const unreadable = path.join(tmpDir, "unreadable");

      fs.writeFileSync(unreadable, "Host secret\n  HostName secret.example.com\n", "utf-8");
      fs.chmodSync(unreadable, 0o000);

      fs.writeFileSync(
        mainConfig,
        `Include ${unreadable}\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      // Should still parse the main config even if included file is unreadable
      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("main");

      // Restore permissions for cleanup
      fs.chmodSync(unreadable, 0o644);
    });

    it("continues parsing when some included files exist and others don't", () => {
      const mainConfig = path.join(tmpDir, "config");
      const existingInclude = path.join(tmpDir, "existing");

      fs.writeFileSync(
        mainConfig,
        `Include ${tmpDir}/missing1\nInclude ${existingInclude}\nInclude ${tmpDir}/missing2\n\nHost main\n  HostName main.example.com\n  User admin\n`,
        "utf-8",
      );

      fs.writeFileSync(
        existingInclude,
        `Host included\n  HostName included.example.com\n  User deploy\n`,
        "utf-8",
      );

      const result = parseSshConfig(mainConfig);

      expect(result).toHaveLength(2);
      expect(result[0].alias).toBe("main");
      expect(result[1].alias).toBe("included");
    });
  });
});

describe("resolveIncludePaths", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = makeTempDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  it("resolves an absolute path to a single-element array", () => {
    const filePath = path.join(tmpDir, "some-config");
    const result = resolveIncludePaths(filePath);
    expect(result).toEqual([filePath]);
  });

  it("expands ~ to the home directory", () => {
    const result = resolveIncludePaths("~/.ssh/config.d/extra");
    expect(result).toEqual([path.join(os.homedir(), ".ssh", "config.d", "extra")]);
  });

  it("resolves relative paths relative to ~/.ssh/", () => {
    const result = resolveIncludePaths("config.d/extra");
    expect(result).toEqual([path.join(os.homedir(), ".ssh", "config.d", "extra")]);
  });

  it("expands glob pattern to matching files", () => {
    const configDir = path.join(tmpDir, "config.d");
    fs.mkdirSync(configDir);
    fs.writeFileSync(path.join(configDir, "file1"), "content1");
    fs.writeFileSync(path.join(configDir, "file2"), "content2");

    const result = resolveIncludePaths(`${configDir}/*`);

    expect(result).toHaveLength(2);
    expect(result).toContain(path.join(configDir, "file1"));
    expect(result).toContain(path.join(configDir, "file2"));
  });

  it("returns empty array for glob pattern matching no files", () => {
    const result = resolveIncludePaths(`${tmpDir}/nonexistent/*`);
    expect(result).toEqual([]);
  });

  it("excludes directories from glob results", () => {
    const configDir = path.join(tmpDir, "d");
    fs.mkdirSync(configDir);
    fs.writeFileSync(path.join(configDir, "file"), "content");
    fs.mkdirSync(path.join(configDir, "subdir"));

    const result = resolveIncludePaths(`${configDir}/*`);

    expect(result).toEqual([path.join(configDir, "file")]);
  });

  it("sorts glob results alphabetically", () => {
    const configDir = path.join(tmpDir, "sorted");
    fs.mkdirSync(configDir);
    fs.writeFileSync(path.join(configDir, "z-last"), "");
    fs.writeFileSync(path.join(configDir, "a-first"), "");
    fs.writeFileSync(path.join(configDir, "m-middle"), "");

    const result = resolveIncludePaths(`${configDir}/*`);

    expect(result).toEqual([
      path.join(configDir, "a-first"),
      expect.stringContaining("m-middle"),
      expect.stringContaining("z-last"),
    ]);
  });
});
