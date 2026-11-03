import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";

import {
  readWorkspaceConfig,
  writeWorkspaceConfig,
  defaultWorkspaceConfig,
  type WorkspaceConfig,
} from "../electron/workspace-config";

describe("workspace-config", () => {
  let tmpDir: string;

  beforeEach(async () => {
    tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "ws-config-test-"));
  });

  afterEach(async () => {
    await fs.rm(tmpDir, { recursive: true, force: true });
  });

  describe("defaultWorkspaceConfig", () => {
    it("returns version 1 with expected defaults", () => {
      const config = defaultWorkspaceConfig();

      expect(config.version).toBe(1);
      expect(config.discovery.maxDepth).toBe(3);
      expect(config.discovery.excludePatterns).toEqual([
        "node_modules",
        ".git",
        "vendor",
        "target",
        "build",
      ]);
      expect(config.repos.removed).toEqual([]);
      expect(config.display.tabBarState).toEqual([]);
    });

    it("returns a fresh object each call (no shared references)", () => {
      const a = defaultWorkspaceConfig();
      const b = defaultWorkspaceConfig();
      a.discovery.excludePatterns.push("extra");
      expect(b.discovery.excludePatterns).not.toContain("extra");
    });
  });

  describe("readWorkspaceConfig", () => {
    it("returns null when config file does not exist", async () => {
      const result = await readWorkspaceConfig(tmpDir);
      expect(result).toBeNull();
    });

    it("returns null when .crab directory does not exist", async () => {
      const result = await readWorkspaceConfig(path.join(tmpDir, "nonexistent"));
      expect(result).toBeNull();
    });

    it("reads and parses a valid config file", async () => {
      const config: WorkspaceConfig = {
        version: 1,
        discovery: { maxDepth: 5, excludePatterns: ["dist"] },
        repos: { removed: ["old-repo"] },
        display: { tabBarState: ["frontend", "backend"] },
      };

      const crabDir = path.join(tmpDir, ".crab");
      await fs.mkdir(crabDir, { recursive: true });
      await fs.writeFile(
        path.join(crabDir, "workspace.json"),
        JSON.stringify(config),
        "utf-8",
      );

      const result = await readWorkspaceConfig(tmpDir);
      expect(result).toEqual(config);
    });

    it("returns defaults on malformed JSON", async () => {
      const crabDir = path.join(tmpDir, ".crab");
      await fs.mkdir(crabDir, { recursive: true });
      await fs.writeFile(
        path.join(crabDir, "workspace.json"),
        "{ not valid json !!!",
        "utf-8",
      );

      const result = await readWorkspaceConfig(tmpDir);
      expect(result).not.toBeNull();
      expect(result).toEqual(defaultWorkspaceConfig());
    });

    it("returns defaults when JSON is valid but schema is wrong", async () => {
      const crabDir = path.join(tmpDir, ".crab");
      await fs.mkdir(crabDir, { recursive: true });
      await fs.writeFile(
        path.join(crabDir, "workspace.json"),
        JSON.stringify({ version: 2, unexpected: true }),
        "utf-8",
      );

      const result = await readWorkspaceConfig(tmpDir);
      expect(result).toEqual(defaultWorkspaceConfig());
    });

    it("returns defaults when discovery section is missing", async () => {
      const crabDir = path.join(tmpDir, ".crab");
      await fs.mkdir(crabDir, { recursive: true });
      await fs.writeFile(
        path.join(crabDir, "workspace.json"),
        JSON.stringify({ version: 1, repos: { removed: [] } }),
        "utf-8",
      );

      const result = await readWorkspaceConfig(tmpDir);
      expect(result).toEqual(defaultWorkspaceConfig());
    });
  });

  describe("writeWorkspaceConfig", () => {
    it("creates .crab directory and writes config file", async () => {
      const config = defaultWorkspaceConfig();
      await writeWorkspaceConfig(tmpDir, config);

      const configPath = path.join(tmpDir, ".crab", "workspace.json");
      const raw = await fs.readFile(configPath, "utf-8");
      const parsed = JSON.parse(raw);
      expect(parsed).toEqual(config);
    });

    it("overwrites existing config file", async () => {
      const config1 = defaultWorkspaceConfig();
      await writeWorkspaceConfig(tmpDir, config1);

      const config2: WorkspaceConfig = {
        ...config1,
        discovery: { maxDepth: 7, excludePatterns: ["custom"] },
      };
      await writeWorkspaceConfig(tmpDir, config2);

      const configPath = path.join(tmpDir, ".crab", "workspace.json");
      const raw = await fs.readFile(configPath, "utf-8");
      const parsed = JSON.parse(raw);
      expect(parsed.discovery.maxDepth).toBe(7);
      expect(parsed.discovery.excludePatterns).toEqual(["custom"]);
    });

    it("preserves existing .crab directory contents", async () => {
      const crabDir = path.join(tmpDir, ".crab");
      await fs.mkdir(crabDir, { recursive: true });
      await fs.writeFile(path.join(crabDir, "other-file.txt"), "keep me");

      await writeWorkspaceConfig(tmpDir, defaultWorkspaceConfig());

      const otherContent = await fs.readFile(
        path.join(crabDir, "other-file.txt"),
        "utf-8",
      );
      expect(otherContent).toBe("keep me");
    });
  });

  describe("round-trip", () => {
    it("write then read produces identical config", async () => {
      const config: WorkspaceConfig = {
        version: 1,
        discovery: { maxDepth: 4, excludePatterns: ["node_modules", "dist", ".cache"] },
        repos: { removed: ["legacy/old"] },
        display: { tabBarState: ["api", "web", "shared-lib"] },
      };

      await writeWorkspaceConfig(tmpDir, config);
      const result = await readWorkspaceConfig(tmpDir);

      expect(result).toEqual(config);
    });

    it("persists exclusions for discovered repositories", async () => {
      const config: WorkspaceConfig = {
        version: 1,
        discovery: { maxDepth: 2, excludePatterns: [] },
        repos: { removed: ["old/api", "legacy/project"] },
        display: { tabBarState: [] },
      };

      await writeWorkspaceConfig(tmpDir, config);
      const result = await readWorkspaceConfig(tmpDir);

      expect(result!.repos.removed).toEqual(["old/api", "legacy/project"]);
    });
  });
});
