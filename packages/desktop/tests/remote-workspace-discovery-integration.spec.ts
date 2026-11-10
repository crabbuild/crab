import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { RemoteWorkspaceOrchestrator, type DiscoveredRepo } from "../electron/remote-workspace-orchestrator";
import type { RpcRouter } from "../electron/ssh-router";
import type { SshSessionManager } from "../electron/ssh-manager";

// Exercise the production discovery/polling lifecycle at its RPC boundary.
describe("remote workspace orchestration", () => {
  const repos: DiscoveredRepo[] = [{ absolute_path: "/projects/api", relative_path: "api", name: "api" }];
  let router: { sendToRemoteAgent: ReturnType<typeof vi.fn> };
  let sessions: EventEmitter;
  let orchestrator: RemoteWorkspaceOrchestrator;

  beforeEach(() => {
    vi.useFakeTimers();
    router = { sendToRemoteAgent: vi.fn(async (_profile: string, method: string) =>
      method === "discover_repos" ? { repos } : { statuses: [], poll_timestamp: 1 }) };
    sessions = new EventEmitter();
    orchestrator = new RemoteWorkspaceOrchestrator(router as unknown as RpcRouter, sessions as SshSessionManager);
  });

  afterEach(() => {
    orchestrator.dispose();
    vi.useRealTimers();
  });

  it("passes the selected host, root and discovery options to its agent", async () => {
    await orchestrator.discoverRepos("host", "/projects", { maxDepth: 2, excludePatterns: ["vendor"] });
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("host", "discover_repos", {
      root_path: "/projects", max_depth: 2, exclude_patterns: ["vendor"],
    });
  });

  it("emits discovery results with their host and root identity", async () => {
    const discovered = vi.fn();
    orchestrator.on("discovered", discovered);
    await orchestrator.discoverRepos("host", "/projects");
    expect(discovered).toHaveBeenCalledWith({ profileId: "host", rootPath: "/projects", repos });
  });

  it("keeps workspaces on separate hosts independent", async () => {
    await orchestrator.openWorkspace("a", "/projects");
    await orchestrator.openWorkspace("b", "/projects");
    expect(orchestrator.getOpenWorkspaces()).toEqual([
      { profileId: "a", rootPath: "/projects", repos }, { profileId: "b", rootPath: "/projects", repos },
    ]);
  });

  it("re-discovery replaces a host's inventory without duplicating its workspace", async () => {
    await orchestrator.openWorkspace("host", "/projects");
    router.sendToRemoteAgent.mockImplementation(async () => ({ repos: [] }));
    await orchestrator.openWorkspace("host", "/projects");
    expect(orchestrator.getOpenWorkspaces()).toEqual([{ profileId: "host", rootPath: "/projects", repos: [] }]);
  });

  it("polls absolute repository paths and forwards host-scoped status", async () => {
    const updated = vi.fn();
    orchestrator.on("status-update", updated);
    await orchestrator.openWorkspace("host", "/projects");
    await vi.advanceTimersByTimeAsync(30_000);
    expect(router.sendToRemoteAgent).toHaveBeenLastCalledWith("host", "batch_git_status", { repo_paths: ["/projects/api"] });
    expect(updated).toHaveBeenLastCalledWith({ profileId: "host", statuses: [], poll_timestamp: 1 });
  });

  it("closing a host stops only its poller", async () => {
    await orchestrator.openWorkspace("a", "/projects");
    await orchestrator.openWorkspace("b", "/projects");
    orchestrator.closeWorkspace("a");
    router.sendToRemoteAgent.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(router.sendToRemoteAgent.mock.calls.map(([profile]) => profile)).toEqual(["b"]);
  });

  it("disconnect pauses polling and reconnect resumes its owned workspace", async () => {
    await orchestrator.openWorkspace("host", "/projects");
    sessions.emit("ssh:status-changed", { profileId: "host", status: "disconnected" });
    router.sendToRemoteAgent.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(router.sendToRemoteAgent).not.toHaveBeenCalled();
    sessions.emit("ssh:status-changed", { profileId: "host", status: "connected" });
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("host", "batch_git_status", { repo_paths: ["/projects/api"] });
  });

  it("a disconnected profile without an open workspace has no workspace failure", () => {
    expect(() => sessions.emit("ssh:status-changed", { profileId: "unopened", status: "disconnected" })).not.toThrow();
  });

  it("failed polls publish repository errors through the existing status channel", async () => {
    await orchestrator.openWorkspace("host", "/projects");
    const updated = vi.fn(); orchestrator.on("status-update", updated);
    router.sendToRemoteAgent.mockRejectedValue(new Error("Host unavailable"));
    await vi.advanceTimersByTimeAsync(30_000);
    expect(updated).toHaveBeenCalledWith({ profileId: "host", statuses: [{ relative_path: "api", branch: null, dirty_count: 0, ahead: null, behind: null, error: "Host unavailable" }], poll_timestamp: expect.any(Number) });
  });

  it("closing a workspace discards an in-flight poll result", async () => {
    await orchestrator.openWorkspace("host", "/projects");
    await vi.advanceTimersByTimeAsync(0);
    let finish!: (value: unknown) => void;
    router.sendToRemoteAgent.mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    const updated = vi.fn(); orchestrator.on("status-update", updated);
    await vi.advanceTimersByTimeAsync(30_000);
    orchestrator.closeWorkspace("host");
    finish({ statuses: [], poll_timestamp: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(updated).not.toHaveBeenCalled();
  });

  it("a failed discovery does not register a workspace or poller", async () => {
    router.sendToRemoteAgent.mockRejectedValue(new Error("disconnected"));
    await expect(orchestrator.openWorkspace("host", "/projects")).rejects.toThrow("disconnected");
    expect(orchestrator.getOpenWorkspaces()).toEqual([]);
  });
});
