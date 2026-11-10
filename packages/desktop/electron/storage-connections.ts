import { randomUUID } from "node:crypto";
import { readFile, writeFile, rename, mkdir, rm } from "node:fs/promises";
import * as path from "node:path";
import type { SafeStorage } from "electron";
import type { StorageConnection, StorageConnectionInfo } from "./storage-connection-types";

type Encryption = Pick<SafeStorage, "isEncryptionAvailable" | "encryptString" | "decryptString" | "getSelectedStorageBackend">;
type Send = (method: string, params: unknown) => Promise<unknown>;
interface Vault { connections: StorageConnection[]; bindings: Record<string, string> }

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function text(value: unknown): string { return typeof value === "string" ? value.trim() : ""; }

function connection(value: unknown): StorageConnection {
  if (!record(value) || !record(value.credentials)) throw new Error("Enter storage credentials.");
  const provider = value.provider;
  const bucket = text(value.bucket);
  const name = text(value.name);
  const region = text(value.region);
  const endpoint = text(value.endpoint);
  if (!name || name.length > 120) throw new Error("Enter a connection name of up to 120 characters.");
  if (!/^[a-zA-Z0-9][a-zA-Z0-9._-]*$/.test(bucket)) throw new Error("Enter a bucket or container name, without a path.");
  if (endpoint) {
    let url: URL;
    try { url = new URL(endpoint); } catch { throw new Error("Enter an HTTP or HTTPS endpoint URL."); }
    if (!["https:", "http:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      throw new Error("Endpoint URLs must use HTTP or HTTPS and contain no credentials, query, or fragment.");
    }
  }
  const secret = value.credentials;
  let credentials: StorageConnection["credentials"];
  if (provider === "s3") {
    const access_key_id = text(secret.access_key_id), secret_access_key = text(secret.secret_access_key);
    if (!region || !access_key_id || !secret_access_key) throw new Error("S3 requires a signing region, access key ID, and secret access key.");
    credentials = { provider, access_key_id, secret_access_key, session_token: text(secret.session_token) || undefined };
  } else if (provider === "gcs") {
    if (endpoint) throw new Error("GCS uses the endpoint in its service-account configuration.");
    const service_account_key = text(secret.service_account_key);
    let key: unknown;
    try { key = JSON.parse(service_account_key); } catch { throw new Error("Enter valid service-account JSON."); }
    if (!record(key) || key.type !== "service_account" || !text(key.client_email) || !text(key.private_key_id) || !text(key.private_key)) {
      throw new Error("Service-account JSON must include type, client_email, private_key_id, and private_key.");
    }
    credentials = { provider, service_account_key };
  } else if (provider === "azure") {
    const account = text(secret.account), access_key = text(secret.access_key), sas_token = text(secret.sas_token).replace(/^\?/, "");
    if (!/^[a-z0-9]+$/.test(account)) throw new Error("Enter a valid Azure storage account name.");
    if (Boolean(access_key) === Boolean(sas_token)) throw new Error("Enter either an Azure account key or a SAS token.");
    if (access_key && (!/^[A-Za-z0-9+/]+={0,2}$/.test(access_key) || access_key.length % 4 !== 0)) throw new Error("Azure account keys must be base64 encoded.");
    if (sas_token && (!new URLSearchParams(sas_token).get("sig") || sas_token.includes("://"))) throw new Error("Enter the SAS query string including its signature, without a URL.");
    credentials = { provider, account, access_key: access_key || undefined, sas_token: sas_token || undefined };
  } else throw new Error("Choose S3, Google Cloud Storage, or Azure Blob Storage.");
  return { id: text(value.id) || randomUUID(), name, provider, bucket, region, endpoint, credentials };
}

function info({ id, name, provider, bucket, region, endpoint }: StorageConnection): StorageConnectionInfo {
  return { id, name, provider, bucket, region, endpoint };
}

export class StorageConnections {
  private mutations: Promise<unknown> = Promise.resolve();
  private cached: Vault | undefined;
  private inFlight = new Set<StorageConnection>();
  private sessions = new Map<string, { key: string; url: string; remote: boolean }>();

  constructor(private file: string, private encryption: Encryption, private platform: NodeJS.Platform) {}

  private available(): boolean {
    return this.encryption.isEncryptionAvailable() &&
      !(this.platform === "linux" && this.encryption.getSelectedStorageBackend() === "basic_text");
  }

  private requireEncryption(): void {
    if (!this.available()) throw new Error("Unlock or configure your operating system keyring to use saved storage connections.");
  }

  private async read(): Promise<Vault> {
    if (this.cached) return structuredClone(this.cached);
    let bytes: Buffer;
    try { bytes = await readFile(this.file); }
    catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return { connections: [], bindings: {} }; throw error; }
    this.requireEncryption();
    try {
      const data: unknown = JSON.parse(this.encryption.decryptString(bytes));
      if (!record(data) || !Array.isArray(data.connections) || !record(data.bindings)) throw new Error();
      const connections = data.connections.map(connection);
      const bindings: Record<string, string> = {};
      for (const [key, value] of Object.entries(data.bindings)) if (typeof value === "string") bindings[key] = value;
      this.cached = { connections, bindings };
      return structuredClone(this.cached);
    } catch { throw new Error("Could not unlock saved storage connections. Restore the device keyring before trying again."); }
  }

  private mutate<T>(change: (vault: Vault) => T | Promise<T>): Promise<T> {
    const operation = this.mutations.then(async () => {
      this.requireEncryption();
      const vault = await this.read();
      const result = await change(vault);
      const encrypted = this.encryption.encryptString(JSON.stringify(vault));
      await mkdir(path.dirname(this.file), { recursive: true, mode: 0o700 });
      const temporary = `${this.file}.${randomUUID()}`;
      try {
        await writeFile(temporary, encrypted, { mode: 0o600, flag: "wx" });
        await rename(temporary, this.file);
        this.cached = vault;
      } finally { await rm(temporary, { force: true }); }
      return result;
    });
    this.mutations = operation.catch(() => {});
    return operation;
  }

  redactText(message: string): string {
    let redacted = message.replace(/([?&]sig=)[^&\s]+/gi, "$1[redacted]");
    for (const connection of [...(this.cached?.connections ?? []), ...this.inFlight]) {
      const credentials = connection.credentials;
      const secrets: string[] = [];
      if (credentials.provider === "s3") secrets.push(credentials.access_key_id, credentials.secret_access_key, credentials.session_token ?? "");
      if (credentials.provider === "azure") secrets.push(credentials.access_key ?? "", credentials.sas_token ?? "");
      if (credentials.provider === "gcs") {
        secrets.push(credentials.service_account_key);
        const key: unknown = JSON.parse(credentials.service_account_key);
        if (record(key)) secrets.push(text(key.private_key));
      }
      for (const value of secrets) if (value) redacted = redacted.split(value).join("[redacted]");
    }
    return redacted;
  }

  private redactValue(value: unknown): unknown {
    if (typeof value === "string") return this.redactText(value);
    if (Array.isArray(value)) return value.map(item => this.redactValue(item));
    if (record(value)) return Object.fromEntries(Object.entries(value).map(([key, item]) => [this.redactText(key), this.redactValue(item)]));
    return value;
  }

  async reopen(repoId: string, nativeSend: Send): Promise<unknown> {
    const session = this.sessions.get(repoId);
    if (!session) throw new Error("Repository storage session is no longer available");
    // Bindings outlive a native process. Resolve the current selection instead
    // of replaying credentials or an explicit profile saved by an older open.
    const selected = this.cached?.bindings[session.key];
    return this.request(session.remote ? "open_url" : "open_local", {
      repo_id: repoId,
      ...(session.remote ? { url: session.url } : { path: session.key }),
      ...(selected ? { storage_connection_id: selected } : {}),
    }, nativeSend);
  }

  async request(method: string, params: unknown, nativeSend: Send): Promise<unknown> {
    const send: Send = async (name, payload) => {
      const repoId = record(payload) ? text(payload.repo_id) : "";
      const binding = this.cached?.bindings[this.sessions.get(repoId)?.key ?? repoId];
      const selected = record(payload) && record(payload.__storage_auth)
        ? connection(payload.__storage_auth)
        : this.cached?.connections.find(item => item.id === binding);
      // Forgetting a profile must not expose credentials in late diagnostics
      // from operations already using it. Each call owns its redaction lifetime.
      const active = selected ? structuredClone(selected) : undefined;
      if (active) this.inFlight.add(active);
      try { return await nativeSend(name, payload); }
      catch (error) {
        if (!(error instanceof Error)) throw error;
        // Provider failures can contain signed URLs. Strip credential material
        // at the public IPC boundary, including errors from already-open SDKs.
        const detail = error as Error & { code?: number; data?: unknown };
        throw Object.assign(new Error(this.redactText(error.message)), {
          code: detail.code,
          data: this.redactValue(detail.data),
        });
      } finally { if (active) this.inFlight.delete(active); }
    };
    const payload = record(params) ? { ...params } : {};
    // Renderer input never supplies the agent's private credential context.
    delete payload.__storage_auth;
    if (method === "storage_connections_list") {
      const vault = await this.read();
      const repoId = text(payload.repo_id);
      const selected_id = vault.bindings[this.sessions.get(repoId)?.key ?? repoId] ?? null;
      return { connections: vault.connections.map(info), selected_id, secure_storage: this.available() };
    }
    if (method === "storage_connections_save") {
      const next = connection(payload.connection);
      return this.mutate(vault => {
        if (vault.connections.some(item => item.id === next.id) && [...this.sessions.values()].some(session => session.remote && vault.bindings[session.key] === next.id)) {
          throw new Error("Close remote repositories using this connection before replacing its credentials.");
        }
        vault.connections = [...vault.connections.filter(item => item.id !== next.id), next];
        return info(next);
      });
    }
    if (method === "storage_connections_remove") {
      const id = text(payload.id);
      const forgotten = (await this.read()).connections.find(item => item.id === id);
      if (forgotten) this.inFlight.add(forgotten);
      try {
        const bound = await this.mutate(vault => {
          vault.connections = vault.connections.filter(item => item.id !== id);
          // Retain bindings so an existing repo cannot silently switch identity
          // to ambient credentials after its saved connection is forgotten.
          return Object.keys(vault.bindings).filter(key => vault.bindings[key] === id);
        });
        for (const [repo_id, session] of this.sessions) {
          if (session.remote && bound.includes(session.key)) await send("close", { repo_id }).catch(() => {});
        }
        return { ok: true };
      } finally { if (forgotten) this.inFlight.delete(forgotten); }
    }
    if (method === "storage_connections_bind") {
      const repo = text(payload.repo_id), id = text(payload.id);
      const session = this.sessions.get(repo);
      if (!session) throw new Error("Open a repository on this device before selecting its storage connection.");
      if (!id && !(await this.read()).bindings[session.key]) return { ok: true };
      const selected = await this.mutate(vault => {
        const selected = vault.connections.find(item => item.id === id);
        if (id && !selected) throw new Error("Storage connection was not found.");
        if (id) vault.bindings[session.key] = id; else delete vault.bindings[session.key];
        return selected;
      });
      // Persist the requested identity before reconnecting. A failed reconnect
      // must never leave a repository silently using the previous identity.
      if (session.remote) {
        await send("open_url", { repo_id: repo, url: session.url, ...(selected ? { __storage_auth: selected } : {}) });
      }
      return { ok: true };
    }
    if (method === "storage_connections_test") {
      const next = payload.id
        ? (await this.read()).connections.find(item => item.id === payload.id)
        : connection(payload.connection);
      if (!next) throw new Error("Storage connection was not found.");
      return send("storage_connection_test", { __storage_auth: next });
    }
    const repoId = text(payload.repo_id);
    const session = this.sessions.get(repoId);
    const key = session?.key || (typeof payload.path === "string" ? path.resolve(payload.path) : text(payload.cwd));
    const explicit = Object.hasOwn(payload, "storage_connection_id");
    const needsAuth = explicit || (method === "open_local" && !!this.cached?.bindings[key]) || method.startsWith("crab_") || method.startsWith("workflow_") ||
      ["open_url", "clone", "push", "hydrate", "dehydrate", "storage_summary", "storage_config_get", "storage_config_verify", "git_fetch", "git_fetch_branch", "git_fetch_all", "git_pull", "git_pull_branch", "git_pull_mode", "git_push_branch", "git_push_remote", "git_remote_add", "git_remote_set_url"].includes(method);
    // Local browsing and app startup do not depend on an unlocked cloud keyring.
    const vault = needsAuth ? await this.read() : undefined;
    const id = explicit ? text(payload.storage_connection_id) : vault?.bindings[key];
    delete payload.storage_connection_id;
    let selected: StorageConnection | undefined;
    if (id) {
      selected = vault?.connections.find(item => item.id === id);
      if (!selected) throw new Error("This repository's saved connection was removed. Select another connection in Storage settings.");
      payload.__storage_auth = selected;
    }
    // Record explicit identity before opening or cloning. If encrypted storage
    // cannot be written, do not create a cloud session or an unbound checkout.
    if (explicit && (method === "open_local" || method === "open_url")) {
      const openingKey = method === "open_url" ? repoId : path.resolve(text(payload.path));
      if (id || vault?.bindings[openingKey]) {
        await this.mutate(vault => { if (id) vault.bindings[openingKey] = id; else delete vault.bindings[openingKey]; });
      }
    }
    if (method === "clone" && selected && typeof payload.dest === "string") {
      const dest = path.resolve(payload.dest);
      await this.mutate(vault => { vault.bindings[dest] = selected.id; });
    }
    const result = await send(method, payload);
    if ((method === "open_local" || method === "open_url") && record(result) && typeof result.repo_id === "string") {
      const repo_id = result.repo_id;
      const remote = method === "open_url";
      const sessionKey = remote ? repo_id : path.resolve(text(payload.path));
      this.sessions.set(repo_id, { key: sessionKey, url: text(payload.url) || text(result.url), remote });
    }
    if (method === "close" && session) {
      this.sessions.delete(repoId);
      if (session.remote && this.cached?.bindings[session.key]) {
        await this.mutate(vault => { delete vault.bindings[session.key]; });
      }
    }
    return result;
  }
}
