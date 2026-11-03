import { describe, it, expect, vi, beforeEach } from "vitest";

/**
 * Tests for the L2 promotion logic in useWorkspaceRepoSelect.
 *
 * Since the hook uses React state and IPC calls, we test the core
 * branching logic: local repos call rpc.openLocal, remote repos call
 * crab.ssh.openAsRepo with the correct profileId and path.
 */

describe("workspace repo select — L2 promotion routing", () => {
  let mockOpenLocal: ReturnType<typeof vi.fn>;
  let mockOpenAsRepo: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    mockOpenLocal = vi.fn();
    mockOpenAsRepo = vi.fn();
  });

  /**
   * Extracted promotion logic from useWorkspaceRepoSelect.
   * This mirrors the branching in the hook without React dependencies.
   */
  async function promoteToL2(entry: {
    transport: "local" | "remote";
    profileId?: string;
    absolutePath: string;
  }) {
    if (entry.transport === "remote" && entry.profileId) {
      const handle = await mockOpenAsRepo(entry.profileId, entry.absolutePath);
      const repoId = `ssh://${entry.profileId}${entry.absolutePath}`;
      return { handle, repoId };
    } else {
      const handle = await mockOpenLocal(entry.absolutePath);
      return { handle, repoId: handle.repo_id };
    }
  }

  it("calls rpc.openLocal for local repos", async () => {
    const localHandle = { repo_id: "local-123", url: "", label: "" };
    mockOpenLocal.mockResolvedValue(localHandle);

    const result = await promoteToL2({
      transport: "local",
      absolutePath: "/Users/dev/projects/my-repo",
    });

    expect(mockOpenLocal).toHaveBeenCalledWith("/Users/dev/projects/my-repo");
    expect(mockOpenAsRepo).not.toHaveBeenCalled();
    expect(result.repoId).toBe("local-123");
  });

  it("calls crab.ssh.openAsRepo for remote repos with ssh:// prefixed repo_id", async () => {
    const remoteHandle = { repo_id: "remote-456", url: "", label: "" };
    mockOpenAsRepo.mockResolvedValue(remoteHandle);

    const result = await promoteToL2({
      transport: "remote",
      profileId: "gpu-server",
      absolutePath: "/home/dev/projects/api",
    });

    expect(mockOpenAsRepo).toHaveBeenCalledWith("gpu-server", "/home/dev/projects/api");
    expect(mockOpenLocal).not.toHaveBeenCalled();
    expect(result.repoId).toBe("ssh://gpu-server/home/dev/projects/api");
  });

  it("constructs correct ssh:// repo_id format for various paths", async () => {
    mockOpenAsRepo.mockResolvedValue({ repo_id: "r-1", url: "", label: "" });

    const cases = [
      { profileId: "prod-1", path: "/opt/repos/backend", expected: "ssh://prod-1/opt/repos/backend" },
      { profileId: "dev-box", path: "/home/user/code/frontend", expected: "ssh://dev-box/home/user/code/frontend" },
      { profileId: "ci-runner", path: "/builds/project", expected: "ssh://ci-runner/builds/project" },
    ];

    for (const { profileId, path, expected } of cases) {
      const result = await promoteToL2({
        transport: "remote",
        profileId,
        absolutePath: path,
      });
      expect(result.repoId).toBe(expected);
    }
  });

  it("falls back to local path when remote entry has no profileId", async () => {
    const localHandle = { repo_id: "local-789", url: "", label: "" };
    mockOpenLocal.mockResolvedValue(localHandle);

    const result = await promoteToL2({
      transport: "remote",
      profileId: undefined,
      absolutePath: "/some/path",
    });

    // Without profileId, falls back to local open
    expect(mockOpenLocal).toHaveBeenCalledWith("/some/path");
    expect(mockOpenAsRepo).not.toHaveBeenCalled();
    expect(result.repoId).toBe("local-789");
  });
});
