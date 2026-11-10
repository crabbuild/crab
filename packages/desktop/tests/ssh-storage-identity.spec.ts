import { afterEach, expect, it } from "vitest";
import { EventEmitter } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { StorageConnections } from "../electron/storage-connections";
import { RpcRouter } from "../electron/ssh-router";
import type { StorageConnectionInfo } from "../electron/storage-connection-types";

let directory: string | undefined;
afterEach(async () => { if (directory) await rm(directory, { recursive: true, force: true }); });

async function fixture() {
  directory = await mkdtemp(path.join(tmpdir(), "crab-ssh-storage-"));
  // Encryption primitives are qualified in storage-connections.spec.ts; this
  // fixture exercises real vault selection and transport composition.
  const storage = new StorageConnections(path.join(directory, "vault"), {
    isEncryptionAvailable: () => true, getSelectedStorageBackend: () => "gnome_libsecret",
    encryptString: value => Buffer.from(value), decryptString: value => value.toString(),
  }, "darwin", async () => {});
  const frames: Array<{ id: number; method: string; params: Record<string, unknown> }> = [];
  const channel = Object.assign(new EventEmitter(), {
    write(line: string, callback?: (error?: Error | null) => void) {
      const frame = JSON.parse(line) as typeof frames[number]; frames.push(frame); callback?.(null);
      const result = frame.method === "read_text" ? { content: "fixture-secret" } : { repo_id: frame.params.repo_id };
      queueMicrotask(() => channel.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: frame.id, result }) + "\n")));
      return true;
    },
  });
  const manager = { repositoryWork: () => undefined, getStatus: () => "connected", getConnection: () => ({ channels: { agent: channel }, profile: {} }) };
  const router = new RpcRouter(manager as unknown as SshSessionManager, { request: async () => ({}) }, storage);
  router.attachChannel("remote", channel as unknown as ClientChannel);
  const saved = await storage.request("storage_connections_save", { connection: {
    name: "Remote store", provider: "s3", bucket: "fixture", region: "us-east-1", endpoint: "http://127.0.0.1:9000",
    credentials: { provider: "s3", access_key_id: "fixture-access", secret_access_key: "fixture-secret" },
  } }, async () => ({})) as StorageConnectionInfo;
  return { storage, router, frames, saved };
}

it("injects the saved device identity into SSH open and subsequent file capture", async () => {
  const { router, frames, saved } = await fixture();
  const repo_id = "ssh://remote/data/repo";
  await router.sendToRemoteAgent("remote", "open_local", { repo_id, path: "/data/repo", storage_connection_id: saved.id });
  await router.sendToRemoteAgent("remote", "file_open", { repo_id, path: "weights.bin" });
  expect(frames.at(-1)?.params.__storage_auth).toMatchObject({ id: saved.id });
});

it("an unscoped SSH path cannot inherit a local worktree's saved credentials", async () => {
  const { storage, router, frames, saved } = await fixture();
  await storage.request("open_local", { repo_id: "local", path: "/data/repo", storage_connection_id: saved.id }, async () => ({ repo_id: "local" }));
  await router.sendToRemoteAgent("remote", "read_text", { path: "/data/repo" });
  expect(frames.at(-1)?.params.__storage_auth).toBeUndefined();
});

it("SSH transport rejects a repository belonging to another profile before sending credentials", async () => {
  const { router, frames, saved } = await fixture();
  await expect(router.sendToRemoteAgent("remote", "open_local", { repo_id: "ssh://another/data/repo", path: "/data/repo", storage_connection_id: saved.id })).rejects.toThrow("another SSH profile");
  expect(frames).toHaveLength(0);
});

it("successful content bytes are preserved even when they equal credential text", async () => {
  const { router, saved } = await fixture();
  const repo_id = "ssh://remote/data/repo";
  await router.sendToRemoteAgent("remote", "open_local", { repo_id, path: "/data/repo", storage_connection_id: saved.id });
  await expect(router.sendToRemoteAgent("remote", "read_text", { repo_id, path: "example.txt" })).resolves.toEqual({ content: "fixture-secret" });
});
