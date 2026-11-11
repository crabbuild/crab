import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";

import {
  StatusPoller,
  parseGitStatus,
  type RepoStatus,
} from "../electron/workspace-poller";
import { parseRepoId } from "../electron/ssh-types";

const execFile = promisify(execFileCb);

describe("workspace-poller", () => {
  describe("parseGitStatus", () => {
    it("parses branch with tracking and ahead/behind", () => {
      const output = "## main...origin/main [ahead 2, behind 1]\n M file.txt\n?? new.txt\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe("main");
      expect(result.dirtyCount).toBe(2);
      expect(result.ahead).toBe(2);
      expect(result.behind).toBe(1);
    });

    it("parses branch with tracking and only ahead", () => {
      const output = "## feature...origin/feature [ahead 3]\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe("feature");
      expect(result.dirtyCount).toBe(0);
      expect(result.ahead).toBe(3);
      expect(result.behind).toBe(null);
    });

    it("parses branch with tracking and only behind", () => {
      const output = "## develop...origin/develop [behind 5]\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe("develop");
      expect(result.dirtyCount).toBe(0);
      expect(result.ahead).toBe(null);
      expect(result.behind).toBe(5);
    });

    it("parses branch with tracking but no ahead/behind", () => {
      const output = "## main...origin/main\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe("main");
      expect(result.dirtyCount).toBe(0);
      expect(result.ahead).toBe(null);
      expect(result.behind).toBe(null);
    });

    it("parses branch without tracking info", () => {
      const output = "## my-branch\n M dirty.ts\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe("my-branch");
      expect(result.dirtyCount).toBe(1);
      expect(result.ahead).toBe(null);
      expect(result.behind).toBe(null);
    });

    it("parses detached HEAD", () => {
      const output = "## HEAD (no branch)\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe(null);
      expect(result.dirtyCount).toBe(0);
    });

    it("parses 'No commits yet' state", () => {
      const output = "## No commits yet on main\n?? file.txt\n";
      const result = parseGitStatus(output);

      expect(result.branch).toBe("main");
      expect(result.dirtyCount).toBe(1);
    });

    it("handles empty output", () => {
      const result = parseGitStatus("");

      expect(result.branch).toBe(null);
      expect(result.dirtyCount).toBe(0);
      expect(result.ahead).toBe(null);
      expect(result.behind).toBe(null);
    });

    it("counts multiple dirty files correctly", () => {
      const output = [
        "## main...origin/main",
        " M src/a.ts",
        " M src/b.ts",
        "?? src/c.ts",
        "A  src/d.ts",
        "D  src/e.ts",
        "",
      ].join("\n");
      const result = parseGitStatus(output);

      expect(result.branch).toBe("main");
      expect(result.dirtyCount).toBe(5);
    });
  });

  describe("StatusPoller integration", () => {
    let tmpDir: string;

    beforeEach(async () => {
      tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "ws-poller-test-"));
    });

    afterEach(async () => {
      await fs.rm(tmpDir, { recursive: true, force: true });
    });

    /** Helper: create a real git repo at the given relative path */
    async function createGitRepo(relativePath: string): Promise<string> {
      const repoDir = path.join(tmpDir, relativePath);
      await fs.mkdir(repoDir, { recursive: true });
      await execFile("git", ["init"], { cwd: repoDir });
      await execFile("git", ["config", "user.email", "test@test.com"], {
        cwd: repoDir,
      });
      await execFile("git", ["config", "user.name", "Test"], {
        cwd: repoDir,
      });
      // Create an initial commit so we have a branch
      await fs.writeFile(path.join(repoDir, "README.md"), "# Test\n");
      await execFile("git", ["add", "."], { cwd: repoDir });
      await execFile("git", ["commit", "-m", "init"], { cwd: repoDir });
      return repoDir;
    }

    it("polls a real git repo and returns correct status", async () => {
      await createGitRepo("repo-a");

      const poller = new StatusPoller(tmpDir, ["repo-a"]);
      const statuses = await poller.pollNow();

      expect(statuses).toHaveLength(1);
      expect(statuses[0].relativePath).toBe("repo-a");
      expect(statuses[0].branch).toEqual(expect.any(String));
      expect(statuses[0].dirtyCount).toBe(0);
      expect(statuses[0].error).toBe(null);
    });

    it("detects dirty files", async () => {
      const repoDir = await createGitRepo("dirty-repo");
      await fs.writeFile(path.join(repoDir, "new-file.txt"), "hello");

      const poller = new StatusPoller(tmpDir, ["dirty-repo"]);
      const statuses = await poller.pollNow();

      expect(statuses[0].dirtyCount).toBe(1);
    });

    it("handles errors for non-existent repos gracefully", async () => {
      const poller = new StatusPoller(tmpDir, ["does-not-exist"]);
      const statuses = await poller.pollNow();

      expect(statuses).toHaveLength(1);
      expect(statuses[0].relativePath).toBe("does-not-exist");
      expect(statuses[0].error).not.toBe(null);
      expect(statuses[0].branch).toBe(null);
    });

    it("isolates errors: one failing repo does not affect others", async () => {
      await createGitRepo("good-repo");

      const poller = new StatusPoller(tmpDir, ["good-repo", "bad-repo"]);
      const statuses = await poller.pollNow();

      expect(statuses).toHaveLength(2);

      const good = statuses.find((s) => s.relativePath === "good-repo")!;
      const bad = statuses.find((s) => s.relativePath === "bad-repo")!;

      expect(good.error).toBe(null);
      expect(good.branch).toEqual(expect.any(String));

      expect(bad.error).not.toBe(null);
      expect(bad.branch).toBe(null);
    });

    it("respects batch size limiting", async () => {
      // Create 5 repos, use batchSize of 2
      for (let i = 0; i < 5; i++) {
        await createGitRepo(`repo-${i}`);
      }

      const repoPaths = Array.from({ length: 5 }, (_, i) => `repo-${i}`);
      const poller = new StatusPoller(tmpDir, repoPaths, { batchSize: 2 });
      const statuses = await poller.pollNow();

      // All repos should still be polled
      expect(statuses).toHaveLength(5);
      for (const status of statuses) {
        expect(status.error).toBe(null);
        expect(status.branch).toEqual(expect.any(String));
      }
    });

    it("start() triggers an immediate first poll", async () => {
      await createGitRepo("start-repo");

      const poller = new StatusPoller(tmpDir, ["start-repo"], {
        intervalMs: 60000,
      });

      const statusPromise = new Promise<RepoStatus[]>((resolve) => {
        poller.on("status", (statuses) => {
          resolve(statuses);
        });
      });

      poller.start();

      const statuses = await statusPromise;
      expect(statuses).toHaveLength(1);
      expect(statuses[0].branch).toEqual(expect.any(String));

      poller.stop();
    });

    it.each([false, true])("discards an in-flight result after stop (restarted: %s)", async (restart) => {
      const poller = new StatusPoller(tmpDir, []);
      let complete!: (statuses: RepoStatus[]) => void;
      const pending = new Promise<RepoStatus[]>(resolve => { complete = resolve; });
      vi.spyOn(poller, "pollNow").mockReturnValueOnce(pending).mockResolvedValue([]);
      const publish = vi.fn();
      poller.on("status", publish);
      try {
        poller.start();
        poller.stop();
        if (restart) poller.start();
        complete([]);
        await pending;
        // Drain both runs' publication reactions; only the new run may emit.
        await new Promise<void>(resolve => setImmediate(resolve));
        expect(publish.mock.calls).toEqual(restart ? [[[]]] : []);
      } finally { poller.stop(); }
    });

    it("stop() prevents further polling", async () => {
      await createGitRepo("stop-repo");

      const poller = new StatusPoller(tmpDir, ["stop-repo"], {
        intervalMs: 100,
      });

      let emitCount = 0;
      poller.on("status", () => {
        emitCount++;
      });

      poller.start();

      // Wait long enough for the immediate poll to complete
      await new Promise((r) => setTimeout(r, 50));
      poller.stop();

      const countAfterStop = emitCount;
      // Wait well past when the next interval would have fired
      await new Promise((r) => setTimeout(r, 300));

      expect(emitCount).toBe(countAfterStop);
    });

    it("updateRepos() changes the set of polled repos", async () => {
      await createGitRepo("repo-x");
      await createGitRepo("repo-y");

      const poller = new StatusPoller(tmpDir, ["repo-x"]);

      let statuses = await poller.pollNow();
      expect(statuses).toHaveLength(1);
      expect(statuses[0].relativePath).toBe("repo-x");

      poller.updateRepos(["repo-x", "repo-y"]);

      statuses = await poller.pollNow();
      expect(statuses).toHaveLength(2);
      const paths = statuses.map((s) => s.relativePath).sort();
      expect(paths).toEqual(["repo-x", "repo-y"]);
    });

    it("uses default options when none provided", () => {
      const poller = new StatusPoller(tmpDir, []);
      // Verify it doesn't throw and can be started/stopped
      poller.start();
      poller.stop();
    });
  });

  describe("Remote repo classification via parseRepoId", () => {
    it("parseRepoId classifies ssh:// prefixed IDs as remote", () => {
      const result = parseRepoId("ssh://gpu-server/home/dev/projects/api");
      expect(result).not.toBeNull();
      expect(result!.profileId).toBe("gpu-server");
      expect(result!.remotePath).toBe("/home/dev/projects/api");
    });

    it("parseRepoId returns null for local paths", () => {
      expect(parseRepoId("my-repo")).toBeNull();
      expect(parseRepoId("/home/dev/projects/api")).toBeNull();
      expect(parseRepoId("relative/path/repo")).toBeNull();
    });

    it("parseRepoId groups multiple remote repos by profile", () => {
      const repoIds = [
        "ssh://gpu-server/home/dev/api",
        "ssh://gpu-server/home/dev/web",
        "ssh://staging-box/opt/services/auth",
        "local-repo",
      ];

      const localRepos: string[] = [];
      const remoteByProfile = new Map<string, string[]>();

      for (const repoId of repoIds) {
        const parsed = parseRepoId(repoId);
        if (parsed) {
          const group = remoteByProfile.get(parsed.profileId) ?? [];
          group.push(repoId);
          remoteByProfile.set(parsed.profileId, group);
        } else {
          localRepos.push(repoId);
        }
      }

      expect(localRepos).toEqual(["local-repo"]);
      expect(remoteByProfile.size).toBe(2);
      expect(remoteByProfile.get("gpu-server")).toEqual([
        "ssh://gpu-server/home/dev/api",
        "ssh://gpu-server/home/dev/web",
      ]);
      expect(remoteByProfile.get("staging-box")).toEqual([
        "ssh://staging-box/opt/services/auth",
      ]);
    });

    it("updateRepos accepts mixed local and remote repo IDs", async () => {
      let tmpDir2: string;
      tmpDir2 = await fs.mkdtemp(path.join(os.tmpdir(), "ws-poller-remote-test-"));

      const poller = new StatusPoller(tmpDir2, ["local-repo"]);

      // Simulate what main.ts does: combine local paths with ssh:// remote IDs
      const localPaths = ["local-repo"];
      const remoteRepoIds = [
        "ssh://gpu-server/home/dev/api",
        "ssh://gpu-server/home/dev/web",
      ];

      poller.updateRepos([...localPaths, ...remoteRepoIds]);

      // Poll — local repo will error (doesn't exist), remote repos will
      // error (no provider/router), but the classification should work
      const statuses = await poller.pollNow();

      // All 3 repos should be attempted (local + 2 remote)
      expect(statuses).toHaveLength(3);

      // Local repo should have an error (doesn't exist on disk)
      const localStatus = statuses.find((s) => s.relativePath === "local-repo");
      expect(localStatus).toBeDefined();
      expect(localStatus!.error).not.toBeNull();

      // Remote repos should also have errors (no provider available)
      const remoteStatuses = statuses.filter((s) => s.relativePath.startsWith("ssh://"));
      expect(remoteStatuses).toHaveLength(2);
      for (const rs of remoteStatuses) {
        expect(rs.error).not.toBeNull();
      }

      await fs.rm(tmpDir2, { recursive: true, force: true });
    });
  });
});
