import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";

import {
  discoverRepos,
  type DiscoveredRepo,
} from "../electron/workspace-discovery";

describe("workspace-discovery", () => {
  let tmpDir: string;

  beforeEach(async () => {
    tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "ws-discovery-test-"));
  });

  afterEach(async () => {
    await fs.rm(tmpDir, { recursive: true, force: true });
  });

  /** Helper: create a fake git repo at the given relative path */
  async function createRepo(relativePath: string): Promise<void> {
    const repoDir = path.join(tmpDir, relativePath);
    await fs.mkdir(path.join(repoDir, ".git"), { recursive: true });
  }

  /** Helper: create a plain directory (not a repo) */
  async function createDir(relativePath: string): Promise<void> {
    await fs.mkdir(path.join(tmpDir, relativePath), { recursive: true });
  }

  describe("basic discovery", () => {
    it("discovers repos at depth 1", async () => {
      await createRepo("repo-a");
      await createRepo("repo-b");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(2);
      const names = results.map((r) => r.name).sort();
      expect(names).toEqual(["repo-a", "repo-b"]);
    });

    it("returns correct absolutePath, relativePath, and name", async () => {
      await createRepo("my-project");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(1);
      expect(results[0].absolutePath).toBe(path.join(tmpDir, "my-project"));
      expect(results[0].relativePath).toBe("my-project");
      expect(results[0].name).toBe("my-project");
    });

    it("discovers nested repos", async () => {
      await createRepo("org/frontend");
      await createRepo("org/backend");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(2);
      const relativePaths = results.map((r) => r.relativePath).sort();
      expect(relativePaths).toEqual([path.join("org", "backend"), path.join("org", "frontend")]);
    });

    it("does not recurse into discovered repos", async () => {
      await createRepo("parent-repo");
      // Create a nested .git inside the discovered repo — should not be found
      await fs.mkdir(path.join(tmpDir, "parent-repo", "sub", ".git"), {
        recursive: true,
      });

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(1);
      expect(results[0].relativePath).toBe("parent-repo");
    });

    it("returns empty array when no repos exist", async () => {
      await createDir("just-a-folder");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(0);
    });

    it("does not include the root itself even if it has .git", async () => {
      await fs.mkdir(path.join(tmpDir, ".git"), { recursive: true });
      await createRepo("child-repo");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(1);
      expect(results[0].relativePath).toBe("child-repo");
    });
  });

  describe("depth limit", () => {
    it("respects maxDepth of 1", async () => {
      await createRepo("shallow");
      await createRepo("deep/nested");

      const results = await discoverRepos({ rootPath: tmpDir, maxDepth: 1 });

      expect(results).toHaveLength(1);
      expect(results[0].relativePath).toBe("shallow");
    });

    it("respects maxDepth of 2", async () => {
      await createRepo("level1");
      await createRepo("a/level2");
      await createRepo("a/b/level3");

      const results = await discoverRepos({ rootPath: tmpDir, maxDepth: 2 });

      const relativePaths = results.map((r) => r.relativePath).sort();
      expect(relativePaths).toEqual([path.join("a", "level2"), "level1"]);
    });

    it("defaults to maxDepth 3", async () => {
      // depth 1: a, depth 2: a/b, depth 3: a/b/repo — within default maxDepth 3
      await createRepo("a/b/repo");
      // depth 4: a/b/c/too-deep — beyond default maxDepth 3
      await createRepo("a/b/c/too-deep");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(1);
      expect(results[0].relativePath).toBe(path.join("a", "b", "repo"));
    });

    it("does not discover repos beyond maxDepth", async () => {
      await createRepo("a/b/c/d/too-deep");

      const results = await discoverRepos({ rootPath: tmpDir, maxDepth: 3 });

      expect(results).toHaveLength(0);
    });
  });

  describe("exclusion patterns", () => {
    it("excludes directories matching patterns", async () => {
      await createRepo("node_modules/some-pkg");
      await createRepo("real-repo");

      const results = await discoverRepos({
        rootPath: tmpDir,
        excludePatterns: ["node_modules"],
      });

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe("real-repo");
    });

    it("skips excluded directories and all descendants", async () => {
      await createRepo("vendor/lib-a");
      await createRepo("vendor/lib-b/nested");
      await createRepo("src/app");

      const results = await discoverRepos({
        rootPath: tmpDir,
        excludePatterns: ["vendor"],
      });

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe("app");
    });

    it("supports glob patterns", async () => {
      await createRepo("build-output/pkg");
      await createRepo("build-cache/pkg");
      await createRepo("source/pkg");

      const results = await discoverRepos({
        rootPath: tmpDir,
        excludePatterns: ["build*"],
      });

      expect(results).toHaveLength(1);
      expect(results[0].relativePath).toBe(path.join("source", "pkg"));
    });

    it("uses default exclude patterns when none specified", async () => {
      await createRepo("node_modules/dep");
      await createRepo("vendor/lib");
      await createRepo("target/debug");
      await createRepo("build/out");
      await createRepo("real-project");

      const results = await discoverRepos({ rootPath: tmpDir });

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe("real-project");
    });

    it("matches only directory name, not full path", async () => {
      await createRepo("projects/node_modules_backup");

      const results = await discoverRepos({
        rootPath: tmpDir,
        excludePatterns: ["node_modules"],
      });

      // "node_modules_backup" should NOT be excluded by "node_modules" pattern
      expect(results).toHaveLength(1);
    });
  });

  describe("removed repos", () => {
    it("filters out repos in the removed list", async () => {
      await createRepo("keep-me");
      await createRepo("remove-me");

      const results = await discoverRepos(
        { rootPath: tmpDir },
        { removed: ["remove-me"] },
      );

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe("keep-me");
    });

    it("handles nested removed paths", async () => {
      await createRepo("org/repo-a");
      await createRepo("org/repo-b");

      const results = await discoverRepos(
        { rootPath: tmpDir },
        { removed: [path.join("org", "repo-b")] },
      );

      expect(results).toHaveLength(1);
      expect(results[0].relativePath).toBe(path.join("org", "repo-a"));
    });
  });

  describe("added repos", () => {
    it("includes manually added repos", async () => {
      await createRepo("discovered");
      // Create an "added" repo outside normal discovery
      const addedDir = path.join(tmpDir, "hidden", "added-repo");
      await fs.mkdir(path.join(addedDir, ".git"), { recursive: true });

      const results = await discoverRepos(
        { rootPath: tmpDir, excludePatterns: ["hidden"] },
        { added: ["hidden/added-repo"] },
      );

      expect(results).toHaveLength(2);
      const names = results.map((r) => r.name).sort();
      expect(names).toEqual(["added-repo", "discovered"]);
    });

    it("does not duplicate repos that are both discovered and added", async () => {
      await createRepo("my-repo");

      const results = await discoverRepos(
        { rootPath: tmpDir },
        { added: ["my-repo"] },
      );

      expect(results).toHaveLength(1);
    });

    it("does not include added repos that are also in removed", async () => {
      await createRepo("other");

      const results = await discoverRepos(
        { rootPath: tmpDir },
        { added: ["some-repo"], removed: ["some-repo"] },
      );

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe("other");
    });

    it("resolves relative paths with parent traversal", async () => {
      const results = await discoverRepos(
        { rootPath: tmpDir },
        { added: ["../outside-repo"] },
      );

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe("outside-repo");
      expect(results[0].absolutePath).toBe(
        path.resolve(tmpDir, "../outside-repo"),
      );
    });
  });

  describe("error handling", () => {
    it("skips directories with permission errors gracefully", async () => {
      await createRepo("accessible-repo");
      const restrictedDir = path.join(tmpDir, "restricted");
      await fs.mkdir(restrictedDir);
      await fs.chmod(restrictedDir, 0o000);

      try {
        const results = await discoverRepos({ rootPath: tmpDir });
        // Should still find the accessible repo
        const names = results.map((r) => r.name);
        expect(names).toContain("accessible-repo");
      } finally {
        // Restore permissions for cleanup
        await fs.chmod(restrictedDir, 0o755);
      }
    });
  });
});
