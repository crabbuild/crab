/**
 * Verifies that the commit composer's gitCommit RPC call routes
 * correctly for remote repos with ssh:// prefixed repo_ids.
 *
 * The CommitComposer calls `rpc.gitCommit(repo.repo_id, { message, ... })`
 * which translates to `call("git_commit", { repo_id, message, ... })`.
 * The main process handler extracts repo_id from params and routes
 * through the ProviderRegistry / RpcRouter for ssh:// prefixed IDs.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import { RpcRouter, parseRepoId, routeRpcCall } from "../electron/ssh-router";

// ─── Mocks ───────────────────────────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();
  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

class MockSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockChannel }; profile?: { operationTimeoutMs?: number } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { channels: { agent?: MockChannel }; profile?: { operationTimeoutMs?: number } } | undefined {
    return this.connections.get(profileId);
  }
}

class MockLocalAgent {
  calls: Array<{ method: string; params: unknown }> = [];
  nextResult: unknown = { ok: true };

  async request(method: string, params: unknown): Promise<unknown> {
    this.calls.push({ method, params });
    return this.nextResult;
  }
}

function simulateResponse(channel: MockChannel, id: number, result: unknown): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
  channel.emit("data", Buffer.from(response));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("remote commit routing — gitCommit via RpcRouter", () => {
  let sm: MockSessionManager;
  let channel: MockChannel;
  let localAgent: MockLocalAgent;
  let router: RpcRouter;

  beforeEach(() => {
    sm = new MockSessionManager();
    channel = new MockChannel();
    localAgent = new MockLocalAgent();
    router = new RpcRouter(sm as any, localAgent);
  });

  it("routes git_commit to remote agent when repo_id has ssh:// prefix", async () => {
    sm.setStatus("gpu-server", "connected");
    sm.setConnection("gpu-server", channel);
    router.attachChannel("gpu-server", channel.asChannel());

    // Simulate what the CommitComposer does:
    // rpc.gitCommit(repo.repo_id, { message, amend, sign_off, sign })
    // which becomes: call("git_commit", { repo_id, message, ... })
    const commitParams = {
      repo_id: "ssh://gpu-server/home/dev/my-project",
      message: "feat: add remote commit support",
      amend: false,
      sign_off: true,
    };

    const promise = routeRpcCall(router, localAgent, "git_commit", commitParams);

    // Verify the request was sent to the remote channel, not the local agent
    expect(localAgent.calls).toHaveLength(0);
    expect(channel.written).toHaveLength(1);

    // Verify the NDJSON frame contains the correct method and params
    const frame = JSON.parse(channel.written[0].trim());
    expect(frame.jsonrpc).toBe("2.0");
    expect(frame.method).toBe("git_commit");
    expect(frame.params).toEqual(commitParams);

    // Simulate successful commit response from remote agent
    simulateResponse(channel, frame.id, { sha: "abc1234", subject: "feat: add remote commit support", amend: false });

    const result = await promise;
    expect(result).toEqual({ sha: "abc1234", subject: "feat: add remote commit support", amend: false });
  });

  it("routes git_commit to local agent when repo_id is a local path", async () => {
    localAgent.nextResult = { sha: "def5678", subject: "local commit", amend: false };

    const commitParams = {
      repo_id: "/Users/dev/local-project",
      message: "fix: local change",
    };

    const result = await routeRpcCall(router, localAgent, "git_commit", commitParams);

    // Should go to local agent, not remote
    expect(localAgent.calls).toHaveLength(1);
    expect(localAgent.calls[0].method).toBe("git_commit");
    expect(localAgent.calls[0].params).toEqual(commitParams);
    expect(result).toEqual({ sha: "def5678", subject: "local commit", amend: false });
  });

  it("rejects git_commit during reconnection (write method)", async () => {
    sm.setStatus("gpu-server", "reconnecting");

    const commitParams = {
      repo_id: "ssh://gpu-server/home/dev/my-project",
      message: "this should fail",
    };

    await expect(
      routeRpcCall(router, localAgent, "git_commit", commitParams),
    ).rejects.toMatchObject({ code: "SSH_RECONNECTING" });
  });

  it("rejects git_commit when SSH connection is disconnected", async () => {
    sm.setStatus("gpu-server", "disconnected");

    const commitParams = {
      repo_id: "ssh://gpu-server/home/dev/my-project",
      message: "this should fail",
    };

    await expect(
      routeRpcCall(router, localAgent, "git_commit", commitParams),
    ).rejects.toThrow("SSH connection not available");
  });

  it("parseRepoId correctly identifies remote repo_id used by CommitComposer", () => {
    // The CommitComposer passes repo.repo_id which for remote repos is ssh://<profileId>/<path>
    const remoteRepoId = "ssh://my-server/home/user/project";
    const parsed = parseRepoId(remoteRepoId);

    expect(parsed).not.toBeNull();
    expect(parsed!.profileId).toBe("my-server");
    expect(parsed!.remotePath).toBe("/home/user/project");

    // Local repo_id should not be parsed as remote
    expect(parseRepoId("/home/user/local-project")).toBeNull();
  });

  it("handles amend commit on remote repo", async () => {
    sm.setStatus("dev-box", "connected");
    sm.setConnection("dev-box", channel);
    router.attachChannel("dev-box", channel.asChannel());

    const commitParams = {
      repo_id: "ssh://dev-box/workspace/app",
      message: "fix: corrected typo",
      amend: true,
      sign: true,
    };

    const promise = routeRpcCall(router, localAgent, "git_commit", commitParams);

    const frame = JSON.parse(channel.written[0].trim());
    expect(frame.method).toBe("git_commit");
    expect(frame.params.amend).toBe(true);
    expect(frame.params.sign).toBe(true);

    simulateResponse(channel, frame.id, { sha: "amended123", subject: "fix: corrected typo", amend: true });

    const result = await promise;
    expect(result).toEqual({ sha: "amended123", subject: "fix: corrected typo", amend: true });
  });
});
