import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from "vitest";
import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { StorageConnections } from "../electron/storage-connections";
import type { StorageConnectionInfo, StorageConnectionInput, StorageConnectionStatus } from "../electron/storage-connection-types";
import type { SafeStorage } from "electron";

const fixture = (name = "Store", endpoint = "http://127.0.0.1:9000"): StorageConnectionInput => ({
  name, provider: "s3", bucket: "test-bucket", region: "test-region", endpoint,
  credentials: { provider: "s3", access_key_id: `fixture-access-${name}`, secret_access_key: `fixture-secret-${name}` },
});

function encryption() {
  const key = randomBytes(32);
  return {
    isEncryptionAvailable: vi.fn(() => true), getSelectedStorageBackend: vi.fn((): ReturnType<SafeStorage["getSelectedStorageBackend"]> => "gnome_libsecret"),
    encryptString: vi.fn((value: string) => {
      const iv = randomBytes(12), cipher = createCipheriv("aes-256-gcm", key, iv);
      const encrypted = Buffer.concat([cipher.update(value), cipher.final()]);
      return Buffer.concat([iv, cipher.getAuthTag(), encrypted]);
    }),
    decryptString: vi.fn((value: Buffer) => {
      const decipher = createDecipheriv("aes-256-gcm", key, value.subarray(0, 12));
      decipher.setAuthTag(value.subarray(12, 28));
      return Buffer.concat([decipher.update(value.subarray(28)), decipher.final()]).toString();
    }),
  };
}

describe("built-in storage connections", () => {
  let directory: string;
  let file: string;
  let crypto: ReturnType<typeof encryption>;
  let store: StorageConnections;
  let send: Mock<[method: string, params: unknown], Promise<unknown>>;
  let changed: Mock<[keys: string[], state: "changed" | "removed"], Promise<void>>;
  const call = (method: string, params: unknown = {}) => store.request(method, params, send);
  const save = (connection = fixture()) => call("storage_connections_save", { connection }) as Promise<StorageConnectionInfo>;
  const list = (repo_id?: string) => call("storage_connections_list", { repo_id }) as Promise<StorageConnectionStatus>;

  beforeEach(async () => {
    directory = await mkdtemp(path.join(tmpdir(), "crab-storage-auth-"));
    file = path.join(directory, "connections.enc");
    crypto = encryption();
    store = new StorageConnections(file, crypto, "darwin", (keys, state) => changed(keys, state));
    changed = vi.fn(async (_keys: string[], _state: "changed" | "removed"): Promise<void> => {});
    send = vi.fn(async (method: string, params: unknown): Promise<unknown> => {
      const payload = params as Record<string, unknown>;
      if (method === "open_local" || method === "open_url") return { repo_id: payload.repo_id, url: payload.url ?? "crab://test-bucket/repo", is_remote: method === "open_url" };
      return { ok: true, latency_ms: 2 };
    });
  });
  afterEach(async () => { await rm(directory, { recursive: true, force: true }); });

  it("isolates storage bindings for identical remote paths on different SSH profiles", async () => {
    const first = await save(fixture("First"));
    const second = await save(fixture("Second"));
    await call("open_local", { repo_id: "ssh://first/data/repo", path: "/data/repo", storage_connection_id: first.id });
    await call("open_local", { repo_id: "ssh://second/data/repo", path: "/data/repo", storage_connection_id: second.id });
    send.mockClear();
    await call("file_open", { repo_id: "ssh://first/data/repo", path: "weights.bin" });
    expect(send).toHaveBeenCalledWith("file_open", expect.objectContaining({ __storage_auth: expect.objectContaining({ id: first.id }) }));
  });

  it("encrypts saved credentials and returns only connection metadata", async () => {
    const saved = await save();
    const bytes = await readFile(file);
    expect(bytes.toString()).not.toContain("fixture-secret");
    expect(JSON.stringify(await list())).not.toContain("fixture-secret");
    expect(saved).not.toHaveProperty("credentials");
    expect(send).not.toHaveBeenCalled();
    if (process.platform !== "win32") expect((await stat(file)).mode & 0o777).toBe(0o600);
  });

  it("loads encrypted connections after an application restart", async () => {
    const saved = await save();
    store = new StorageConnections(file, crypto, "darwin", (keys, state) => changed(keys, state));
    expect((await list()).connections).toEqual([saved]);
  });

  it("retains a snapshot binding after close and restores it through a cold vault", async () => {
    const saved = await save();
    await call("open_url", { repo_id: "saved-snapshot", url: "crab://test-bucket/repo", storage_connection_id: saved.id });
    await call("close", { repo_id: "saved-snapshot" });
    store = new StorageConnections(file, crypto, "darwin", (keys, state) => changed(keys, state));
    send.mockClear();
    await call("open_url", { repo_id: "saved-snapshot", url: "crab://test-bucket/repo" });
    expect(send).toHaveBeenCalledWith("open_url", expect.objectContaining({ __storage_auth: expect.objectContaining({ id: saved.id }) }));
  });

  it("reopens a native repository with its current binding instead of its original selection", async () => {
    const first = await save(fixture("First"));
    const second = await save(fixture("Second"));
    await call("open_local", { repo_id: "local", path: directory, storage_connection_id: first.id });
    await call("storage_connections_bind", { repo_id: "local", id: second.id });
    send.mockClear();
    await call("open_local", { repo_id: "local", path: directory });
    expect(send).toHaveBeenCalledWith("open_local", expect.objectContaining({ repo_id: "local", __storage_auth: expect.objectContaining({ id: second.id }) }));
  });

  it("does not reopen a removed storage identity with ambient credentials", async () => {
    const saved = await save();
    await call("open_local", { repo_id: "local", path: directory, storage_connection_id: saved.id });
    await call("storage_connections_remove", { id: saved.id });
    send.mockClear();
    await expect(call("open_local", { repo_id: "local", path: directory })).rejects.toThrow("saved connection was removed");
    expect(send).not.toHaveBeenCalled();
  });

  it("reopens unbound local browsing while the keyring is locked", async () => {
    await call("open_local", { repo_id: "local", path: directory });
    crypto.isEncryptionAvailable.mockReturnValue(false);
    send.mockClear();
    await call("open_local", { repo_id: "local", path: directory });
    expect(send).toHaveBeenCalledWith("open_local", { repo_id: "local", path: directory });
  });

  it("rejects Linux's plaintext-key backend while preserving local browsing", async () => {
    crypto.getSelectedStorageBackend.mockReturnValue("basic_text");
    store = new StorageConnections(file, crypto, "linux", (keys, state) => changed(keys, state));
    await expect(save()).rejects.toThrow("keyring");
    await call("open_local", { repo_id: "local", path: directory });
    await call("list", { repo_id: "local" });
    expect(send).toHaveBeenLastCalledWith("list", { repo_id: "local" });
  });

  it("does not require a keyring to select default credentials in a fresh repo", async () => {
    crypto.isEncryptionAvailable.mockReturnValue(false);
    await call("open_local", { repo_id: "local", path: directory });
    await expect(call("storage_connections_bind", { repo_id: "local", id: null })).resolves.toEqual({ ok: true });
  });

  it.each([
    { ...fixture(), bucket: "bucket/path" },
    { ...fixture(), endpoint: "https://user:secret@storage.invalid" },
    { ...fixture(), endpoint: "https://storage.invalid?token=fixture" },
    { ...fixture(), region: "" },
    { ...fixture(), credentials: { provider: "s3", access_key_id: "id", secret_access_key: "" } },
    { ...fixture(), provider: "azure", credentials: { provider: "azure", account: "account", access_key: "YWJj", sas_token: "sig=fixture" } },
    { ...fixture(), provider: "gcs", endpoint: "", credentials: { provider: "gcs", service_account_key: "not-json" } },
  ])("rejects invalid provider input without forwarding credentials", async connection => {
    await expect(call("storage_connections_test", { connection })).rejects.toThrow();
    expect(send).not.toHaveBeenCalled();
  });

  it.each([
    { name: "GCS", provider: "gcs", credentials: { provider: "gcs", service_account_key: JSON.stringify({ type: "service_account", client_email: "fixture@example.invalid", private_key_id: "fixture-key-id", private_key: "fixture-not-a-real-private-key" }) } },
    { name: "Azure key", provider: "azure", credentials: { provider: "azure", account: "account", access_key: "YWJj" } },
    { name: "Azure SAS", provider: "azure", credentials: { provider: "azure", account: "account", sas_token: "?sp=rl&sig=fixture-signature" } },
  ] as const)("persists and tests $name using its own provider context", async provider => {
    const saved = await save({ ...fixture(), ...provider, region: "", endpoint: "" });
    store = new StorageConnections(file, crypto, "darwin", (keys, state) => changed(keys, state));
    await call("storage_connections_test", { id: saved.id });
    const payload = send.mock.calls.at(-1)![1] as { __storage_auth: StorageConnectionInput };
    expect(payload.__storage_auth.credentials.provider).toBe(provider.provider);
    expect((await list()).connections[0]).not.toHaveProperty("credentials");
  });

  it("does not lose a saved connection when two saves overlap", async () => {
    await Promise.all([save(fixture("First")), save(fixture("Second"))]);
    expect((await list()).connections.map(item => item.name)).toEqual(["First", "Second"]);
  });

  it("keeps simultaneous connections to identical bucket names on distinct endpoints isolated", async () => {
    const first = await save(fixture("First", "https://first.invalid"));
    const second = await save(fixture("Second", "https://second.invalid"));
    await Promise.all([
      call("open_url", { repo_id: "first", url: "crab://test-bucket/repo", storage_connection_id: first.id }),
      call("open_url", { repo_id: "second", url: "crab://test-bucket/repo", storage_connection_id: second.id }),
    ]);
    const contexts = send.mock.calls.map(([, params]) => (params as Record<string, unknown>).__storage_auth as StorageConnectionInput);
    expect(contexts.map(item => item.endpoint)).toEqual(["https://first.invalid", "https://second.invalid"]);
    expect(contexts[0].credentials).toEqual(fixture("First").credentials);
    expect(contexts[1].credentials).toEqual(fixture("Second").credentials);
  });

  it("binds a cloned checkout by filesystem path across new renderer repo IDs and restarts", async () => {
    const saved = await save();
    const dest = path.join(directory, "checkout");
    await call("clone", { url: "crab://test-bucket/repo", dest, storage_connection_id: saved.id });
    store = new StorageConnections(file, crypto, "darwin", (keys, state) => changed(keys, state));
    await call("open_local", { repo_id: "new-renderer-id", path: dest });
    await call("crab_push_structured", { repo_id: "new-renderer-id" });
    expect(send).toHaveBeenLastCalledWith("crab_push_structured", expect.objectContaining({ __storage_auth: expect.objectContaining({ id: saved.id }) }));
  });

  it("selects the destination worktree binding when an existing handle changes path", async () => {
    const first = await save(fixture("First")), second = await save(fixture("Second"));
    const destination = path.join(directory, "destination");
    await call("open_local", { repo_id: "replaced", path: directory, storage_connection_id: first.id });
    await call("open_local", { repo_id: "destination", path: destination, storage_connection_id: second.id });
    send.mockClear();
    await call("open_local", { repo_id: "replaced", path: destination });
    expect(send).toHaveBeenCalledWith("open_local", expect.objectContaining({ __storage_auth: expect.objectContaining({ id: second.id }) }));
  });

  it("restores the logical snapshot binding when replacing a local owner", async () => {
    const first = await save(fixture("First")), second = await save(fixture("Second"));
    await call("open_url", { repo_id: "replaced", url: "crab://test-bucket/repo", storage_connection_id: second.id });
    await call("close", { repo_id: "replaced" });
    await call("open_local", { repo_id: "replaced", path: directory, storage_connection_id: first.id });
    send.mockClear();
    await call("open_url", { repo_id: "replaced", url: "crab://test-bucket/repo" });
    expect(send).toHaveBeenCalledWith("open_url", expect.objectContaining({ __storage_auth: expect.objectContaining({ id: second.id }) }));
  });

  it("never accepts the renderer's private credential-context field", async () => {
    await call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", __storage_auth: fixture() });
    expect(send).toHaveBeenCalledWith("open_url", { repo_id: "repo", url: "crab://test-bucket/repo" });
  });

  it("tests saved credentials without returning them to the renderer", async () => {
    const saved = await save();
    const result = await call("storage_connections_test", { id: saved.id });
    expect(result).toEqual({ ok: true, latency_ms: 2 });
    expect(send).toHaveBeenLastCalledWith("storage_connection_test", { __storage_auth: expect.objectContaining({ credentials: fixture().credentials }) });
  });

  it("delegates selected-identity replacement to the repository lifecycle owner", async () => {
    const first = await save(fixture("First")), second = await save(fixture("Second"));
    await call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", storage_connection_id: first.id });
    await call("storage_connections_bind", { repo_id: "repo", id: second.id });
    expect(changed).toHaveBeenLastCalledWith(["repo"], "changed");
    expect((await list("repo")).selected_id).toBe(second.id);
  });

  it("forgetting credentials closes SDK sessions and prevents an ambient-identity substitution", async () => {
    const saved = await save();
    await call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", storage_connection_id: saved.id });
    await call("storage_connections_remove", { id: saved.id });
    expect(changed).toHaveBeenLastCalledWith(["repo"], "removed");
    await expect(call("crab_push_structured", { repo_id: "repo" })).rejects.toThrow("removed");
  });

  it("redacts selected credentials and signed URLs from public IPC errors", async () => {
    const saved = await save();
    send.mockRejectedValueOnce(Object.assign(new Error("rejected fixture-secret-Store at https://storage.invalid?sig=private-signature"), {
      code: 1012,
      data: { kind: "content_read", code: "integrity", sources: [{ message: "fixture-secret-Store", url: "https://storage.invalid?sig=private-signature&x=1" }], "fixture-access-Store": "detail" },
    }));
    await expect(call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", storage_connection_id: saved.id })).rejects.toMatchObject({
      message: "rejected [redacted] at https://storage.invalid?sig=[redacted]",
      code: 1012,
      data: { kind: "content_read", code: "integrity", sources: [{ message: "[redacted]", url: "https://storage.invalid?sig=[redacted]&x=1" }], "[redacted]": "detail" },
    });
  });
  it("redacts saved credentials from errors emitted by an already-open SDK session", async () => {
    const saved = await save();
    await call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", storage_connection_id: saved.id });
    send.mockRejectedValueOnce(new Error("read failed fixture-secret-Store"));
    await expect(call("read_text", { repo_id: "repo", path: "file" })).rejects.toThrow("read failed [redacted]");
  });

  it("redacts unsaved connection tests while preserving repository content", async () => {
    send.mockImplementationOnce(async () => {
      expect(store.redactText("test fixture-secret-Store")).toBe("test [redacted]");
      throw new Error("test fixture-secret-Store");
    });
    await expect(call("storage_connections_test", { connection: fixture() })).rejects.toThrow("test [redacted]");
    send.mockResolvedValueOnce({ text: "fixture-secret-Store" });
    await expect(call("read_text", { path: "file" })).resolves.toEqual({ text: "fixture-secret-Store" });
  });

  it("keeps the requested identity selected after an SDK reconnect fails", async () => {
    const first = await save(fixture("First")), second = await save(fixture("Second"));
    await call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", storage_connection_id: first.id });
    changed.mockRejectedValueOnce(new Error("provider unavailable"));
    await expect(call("storage_connections_bind", { repo_id: "repo", id: second.id })).rejects.toThrow("unavailable");
    expect((await list("repo")).selected_id).toBe(second.id);
  });

  it("does not open a cloud session when its selected identity cannot be persisted", async () => {
    const saved = await save();
    await rm(file);
    await rm(directory, { recursive: true });
    await writeFile(directory, "not a directory");
    await expect(call("open_url", { repo_id: "repo", url: "crab://test-bucket/repo", storage_connection_id: saved.id })).rejects.toThrow();
    expect(send).not.toHaveBeenCalled();
  });

  it("redacts credentials forgotten while an operation is still running", async () => {
    const saved = await save();
    await call("open_local", { repo_id: "repo", path: directory });
    await call("storage_connections_bind", { repo_id: "repo", id: saved.id });
    let rejectOperation!: (error: Error) => void;
    send.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectOperation = reject; }));
    const operation = call("crab_push_structured", { repo_id: "repo" });
    const rejected = expect(operation).rejects.toThrow("provider rejected [redacted]");
    await vi.waitFor(() => expect(rejectOperation).toBeTypeOf("function"));
    await call("storage_connections_remove", { id: saved.id });
    rejectOperation(new Error("provider rejected fixture-secret-Store"));
    await rejected;
  });

  it("keeps retired credentials redacted through repository cleanup after a profile replacement", async () => {
    const saved = await save(fixture("First"));
    await call("open_local", { repo_id: "repo", path: directory, storage_connection_id: saved.id });
    changed.mockImplementationOnce(async () => {
      throw Object.assign(new Error("cleanup failed fixture-secret-First"), { code: 1012, data: { source: "fixture-secret-First" } });
    });
    await expect(save({ ...fixture("Replacement"), id: saved.id })).rejects.toMatchObject({ message: "cleanup failed [redacted]", code: 1012, data: { source: "[redacted]" } });
  });

  it("allows cold local metadata while locked but never captures content with ambient credentials", async () => {
    const saved = await save();
    await call("open_local", { repo_id: "old", path: directory, storage_connection_id: saved.id });
    store = new StorageConnections(file, crypto, "darwin", (keys, state) => changed(keys, state));
    crypto.isEncryptionAvailable.mockReturnValue(false);
    await call("open_local", { repo_id: "new", path: directory });
    send.mockClear();
    await expect(call("file_open", { repo_id: "new", path: "weights.bin" })).rejects.toThrow("keyring");
    expect(send).not.toHaveBeenCalled();
    crypto.isEncryptionAvailable.mockReturnValue(true);
    await call("file_open", { repo_id: "new", path: "weights.bin" });
    expect(send).toHaveBeenLastCalledWith("file_open", expect.objectContaining({ __storage_auth: expect.objectContaining({ id: saved.id }) }));
  });

});
