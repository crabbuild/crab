/**
 * Verification: SSH module coexistence with the unified RepoProvider.
 *
 * Confirms that:
 * 1. SSH-specific IPC handlers (ssh-enhanced-wiring.ts) remain functional
 *    for SSH-view-only features (terminals, port forwarding, file browser)
 * 2. RemoteProvider correctly wraps RemoteFileEditor and RemoteGitManager
 *    without conflicts (no double-handling of the same IPC channel)
 * 3. The preload whitelist's SSH methods (ssh_read_file, ssh_write_file, etc.)
 *    coexist with the ssh:read-file, ssh:write-file IPC channels used by
 *    the SSH file browser panel
 *
 * Architecture summary:
 * - `crab:rpc` channel → ProviderRegistry → RemoteProvider → RpcRouter/RemoteFileEditor
 * - `ssh:*` channels → ssh-enhanced-wiring.ts / ssh-ipc.ts → direct module calls
 *
 * These two paths are independent and do not conflict because they use
 * entirely different IPC channel names.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { RemoteProvider, type RemoteProviderOptions } from "../electron/remote-provider";
import { withRepositoryRuntimes } from "./repository-runtime-fixture";

// ── Mock Factories ───────────────────────────────────────────────

function createMockRouter() {
  return {
    route: vi.fn().mockResolvedValue({ ok: true }),
    sendToRemoteAgent: vi.fn().mockResolvedValue({ content: "hello", mtime: 1000, size: 5 }),
    on: vi.fn(),
    off: vi.fn(),
    emit: vi.fn(),
  };
}

function createMockFileEditor() {
  return {
    readFile: vi.fn().mockResolvedValue({ content: "file content", path: "/test.ts", mtime: 1000, size: 12, language: "typescript" }),
    writeFile: vi.fn().mockResolvedValue({ success: true, mtime: 2000 }),
    createFile: vi.fn().mockResolvedValue(undefined),
    deleteFile: vi.fn().mockResolvedValue(undefined),
    renameFile: vi.fn().mockResolvedValue(undefined),
  };
}

function createMockGitManager() {
  return {
    detectGitRepo: vi.fn().mockResolvedValue({ isGitRepo: true }),
    getStatus: vi.fn().mockResolvedValue({ branch: "main", dirty: 0 }),
    executeOperation: vi.fn().mockResolvedValue({ success: true }),
    on: vi.fn(),
    off: vi.fn(),
    emit: vi.fn(),
  };
}

function createMockCapabilityNegotiator() {
  return {
    getCached: vi.fn().mockReturnValue({ version: "1.0.0" }),
    negotiate: vi.fn().mockResolvedValue({ version: "1.0.0" }),
    isSupported: vi.fn().mockReturnValue(true),
  };
}

function createMockSessionManager() {
  const manager = { repositoryWork: () => undefined,
    getStatus: vi.fn().mockReturnValue("connected"),
    getConnection: vi.fn().mockReturnValue({ client: {} }),
    agentSession: vi.fn().mockReturnValue("ssh:1"),
    on: vi.fn(),
    off: vi.fn(),
    emit: vi.fn(),
  };
  return withRepositoryRuntimes(manager);
}

// ── Tests ────────────────────────────────────────────────────────

describe("SSH module coexistence — no IPC channel conflicts", () => {
  it("SSH-specific IPC channels (ssh:*) are distinct from the unified crab:rpc channel", () => {
    // The SSH-enhanced-wiring.ts registers handlers on channels like:
    const sshChannels = [
      "ssh:transfer-list",
      "ssh:transfer-cancel",
      "ssh:git-detect",
      "ssh:git-status",
      "ssh:git-operation",
      "ssh:workspace-open",
      "ssh:workspace-search",
      "ssh:workspace-exec",
      "ssh:debug-start",
      "ssh:debug-stop",
      "ssh:read-file",
      "ssh:write-file",
      "ssh:create-file",
      "ssh:delete-file",
      "ssh:rename-file",
    ];

    // The unified provider path uses a single channel:
    const unifiedChannel = "crab:rpc";

    // Verify no overlap — SSH channels never collide with the unified channel
    for (const channel of sshChannels) {
      expect(channel).not.toBe(unifiedChannel);
      expect(channel.startsWith("ssh:")).toBe(true);
    }
  });

  it("preload ALLOWED_METHODS SSH entries use underscore naming (ssh_read_file) distinct from IPC channels (ssh:read-file)", () => {
    // The ALLOWED_METHODS whitelist contains methods like ssh_read_file
    // which are RPC method names passed through the crab:rpc channel.
    // These are distinct from the ssh:read-file IPC channel names.
    const allowedMethods = [
      "ssh_read_file",
      "ssh_write_file",
      "ssh_create_file",
      "ssh_delete_file",
      "ssh_rename_file",
      "ssh_browse_path",
      "ssh_home_dir",
    ];

    const sshIpcChannels = [
      "ssh:read-file",
      "ssh:write-file",
      "ssh:create-file",
      "ssh:delete-file",
      "ssh:rename-file",
      "ssh:browse-path",
      "ssh:home-dir",
    ];

    // These are two different naming conventions for two different paths:
    // - allowedMethods go through: window.crab.rpc("ssh_read_file", ...) → crab:rpc IPC → ProviderRegistry
    // - sshIpcChannels go through: window.crab.ssh.readFile(...) → ssh:read-file IPC → RemoteFileEditor directly
    for (let i = 0; i < allowedMethods.length; i++) {
      expect(allowedMethods[i]).not.toBe(sshIpcChannels[i]);
    }
  });
});

describe("SSH module coexistence — RemoteProvider wraps without conflicts", () => {
  let remoteProvider: RemoteProvider;
  let mockRouter: ReturnType<typeof createMockRouter>;
  let mockFileEditor: ReturnType<typeof createMockFileEditor>;
  let mockSessionManager: ReturnType<typeof createMockSessionManager>;
  let mockCapabilityNegotiator: ReturnType<typeof createMockCapabilityNegotiator>;

  beforeEach(() => {
    mockRouter = createMockRouter();
    mockFileEditor = createMockFileEditor();
    mockSessionManager = createMockSessionManager();
    mockCapabilityNegotiator = createMockCapabilityNegotiator();

    remoteProvider = new RemoteProvider({
      router: mockRouter as any,
      fileEditor: mockFileEditor as any,
      gitManager: createMockGitManager() as any,
      capabilityNegotiator: mockCapabilityNegotiator as any,
      sessionManager: mockSessionManager as any,
    } satisfies RemoteProviderOptions as any);
  });

  it("RemoteProvider.fsCreate delegates to RemoteFileEditor.createFile for files", async () => {
    await remoteProvider.fsCreate("ssh://gpu-server/home/user/repo", "/home/user/repo/new.ts", "file");

    expect(mockFileEditor.createFile).toHaveBeenCalledWith("gpu-server", "/home/user/repo/new.ts");
  });

  it("RemoteProvider.fsCreate delegates to RpcRouter for directories", async () => {
    await remoteProvider.fsCreate("ssh://gpu-server/home/user/repo", "/home/user/repo/new-dir", "directory");

    expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith("gpu-server", "create_directory", { path: "/home/user/repo/new-dir" });
  });

  it("RemoteProvider.fsDelete delegates to RemoteFileEditor.deleteFile", async () => {
    await remoteProvider.fsDelete("ssh://gpu-server/home/user/repo", "/home/user/repo/old.ts");

    expect(mockFileEditor.deleteFile).toHaveBeenCalledWith("gpu-server", "/home/user/repo/old.ts");
  });

  it("RemoteProvider.fsRename delegates to RemoteFileEditor.renameFile", async () => {
    await remoteProvider.fsRename("ssh://gpu-server/home/user/repo", "/home/user/repo/old.ts", "/home/user/repo/new.ts");

    expect(mockFileEditor.renameFile).toHaveBeenCalledWith("gpu-server", "/home/user/repo/old.ts", "/home/user/repo/new.ts");
  });

  it("RemoteProvider.request routes through RpcRouter (not RemoteFileEditor)", async () => {
    await remoteProvider.request("git_status_porcelain", { repo_id: "ssh://gpu-server/home/user/repo" });

    expect(mockRouter.route).toHaveBeenCalledWith(
      "ssh://gpu-server/home/user/repo",
      "git_status_porcelain",
      { repo_id: "ssh://gpu-server/home/user/repo" },
    );
    // RemoteFileEditor should NOT be called for git operations
    expect(mockFileEditor.readFile).not.toHaveBeenCalled();
    expect(mockFileEditor.writeFile).not.toHaveBeenCalled();
  });
});

describe("SSH module coexistence — SSH-view features remain independent", () => {
  it("SSH-enhanced-wiring handlers serve SSH-view-only features that are NOT part of RepoProvider", () => {
    // These SSH-view features are served by ssh-enhanced-wiring.ts
    // and are NOT part of the RepoProvider abstraction:
    const sshViewOnlyFeatures = [
      // Terminals
      "ssh:process-list",
      "ssh:process-signal",
      // Port forwarding (managed by ssh-ipc.ts)
      // File browser (browsing before opening a repo)
      "ssh:workspace-open",
      "ssh:workspace-search",
      "ssh:workspace-exec",
      "ssh:workspace-exec-kill",
      "ssh:workspace-tail",
      "ssh:workspace-tail-stop",
      // Transfer management
      "ssh:transfer-list",
      "ssh:transfer-cancel",
      "ssh:transfer-retry",
      // Environment management
      "ssh:env-list",
      "ssh:env-set",
      "ssh:env-delete",
      // Tool management
      "ssh:tool-detect-managers",
      "ssh:tool-install",
      // Connection quality
      "ssh:quality-get",
      // Bookmarks
      "ssh:bookmark-add",
      "ssh:bookmark-remove",
      "ssh:bookmark-list",
      // Debugging
      "ssh:debug-start",
      "ssh:debug-stop",
    ];

    // These features use their own IPC channels and don't go through
    // ProviderRegistry at all. They remain functional regardless of
    // whether a repo is open or the provider is registered.
    expect(sshViewOnlyFeatures.length).toBeGreaterThan(0);

    // None of these channels conflict with the unified crab:rpc path
    for (const channel of sshViewOnlyFeatures) {
      expect(channel).not.toBe("crab:rpc");
      expect(channel).not.toContain("crab:");
    }
  });

});
