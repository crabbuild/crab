import { randomUUID } from "node:crypto";
import { readFile, writeFile, rename, mkdir, rm, stat } from "node:fs/promises";
import * as path from "node:path";
import { isDeepStrictEqual } from "node:util";
import type { SafeStorage } from "electron";
import type { StorageConnection, StorageConnectionInfo } from "./storage-connection-types";
import { parseRepoId } from "./ssh-types";

type Encryption = Pick<SafeStorage, "isAsyncEncryptionAvailable" | "encryptStringAsync" | "decryptStringAsync" | "getSelectedStorageBackend">;
type Send = (method: string, params: unknown) => Promise<unknown>;
type IdentityChanged = (keys: string[], state: "changed" | "removed") => Promise<void>;
interface Vault { connections: StorageConnection[]; bindings: Record<string, string> }
interface RepositoryBinding {
  key: string;
  location: string;
  generation: number;
  admitted?: number;
}

function selectedConnection(vault: Vault | undefined, key: string): StorageConnection | undefined {
  return vault?.connections.find(item => item.id === vault.bindings[key]);
}

function sameSource(before?: StorageConnection, after?: StorageConnection): boolean {
  if (before === after) return true;
  if (!before || !after) return false;
  const { name: _beforeName, ...oldSource } = before;
  const { name: _afterName, ...newSource } = after;
  return isDeepStrictEqual(oldSource, newSource);
}

function storageSourceChanged(): Error {
  return new Error("Repository storage changed. Run the query after the repository reopens.");
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function text(value: unknown): string { return typeof value === "string" ? value.trim() : ""; }

/** Persist local worktrees by path and isolate remote/profile identities. */
export function storageBindingKey(repoId: string, localPath?: string): string {
  return localPath === undefined || parseRepoId(repoId) ? repoId : path.resolve(localPath);
}

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
  private operations: Promise<unknown> = Promise.resolve();
  private cached: Vault | undefined;
  private inFlight = new Set<StorageConnection>();
  private sessions = new Map<string, RepositoryBinding>();

  constructor(private file: string, private encryption: Encryption, private platform: NodeJS.Platform, private identityChanged: IdentityChanged, private sourceChanged?: (repoIds: string[]) => void) {}

  private async available(): Promise<boolean> {
    if (!await this.encryption.isAsyncEncryptionAvailable()) return false;
    const backend = this.platform === "linux" ? this.encryption.getSelectedStorageBackend() : undefined;
    return backend !== "basic_text" && backend !== "unknown";
  }

  private async hasSavedVault(): Promise<boolean> {
    try { await stat(this.file); return true; }
    catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return false; throw error; }
  }

  private async requireEncryption(): Promise<void> {
    if (!await this.available()) throw new Error("Unlock or configure your operating system keyring to use saved storage connections.");
  }

  private queued<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.operations.then(operation);
    this.operations = result.catch(() => {});
    return result;
  }

  private read(): Promise<Vault> {
    return this.queued(() => this.load());
  }

  private async load(): Promise<Vault> {
    if (this.cached) return structuredClone(this.cached);
    let bytes: Buffer;
    try { bytes = await readFile(this.file); }
    catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return { connections: [], bindings: {} }; throw error; }
    await this.requireEncryption();
    let vault: Vault;
    let rotate: boolean;
    try {
      const decrypted = await this.encryption.decryptStringAsync(bytes);
      const data: unknown = JSON.parse(decrypted.result);
      if (!record(data) || !Array.isArray(data.connections) || !record(data.bindings)) throw new Error();
      const connections = data.connections.map(connection);
      const bindings: Record<string, string> = {};
      for (const [key, value] of Object.entries(data.bindings)) if (typeof value === "string") bindings[key] = value;
      vault = { connections, bindings };
      rotate = decrypted.shouldReEncrypt;
    } catch { throw new Error("Could not unlock saved storage connections. Restore the device keyring before trying again."); }
    // Rotation writes share the mutation queue: a read must not overwrite a
    // connection saved while the native key provider was initializing.
    if (rotate) await this.persist(vault);
    else this.cached = vault;
    return structuredClone(vault);
  }

  private mutate<T>(change: (vault: Vault) => T | Promise<T>): Promise<T> {
    return this.queued(async () => {
      await this.requireEncryption();
      const vault = await this.load();
      const before = { connections: vault.connections, bindings: { ...vault.bindings } };
      const result = await change(vault);
      await this.persist(vault);
      // Publish source generations only after the encrypted vault is durable;
      // an old capture must retire before a request can reuse the new binding.
      const affected: string[] = [];
      for (const [repoId, session] of this.sessions) {
        if (sameSource(selectedConnection(before, session.key), selectedConnection(vault, session.key))) continue;
        session.generation++;
        if (session.admitted !== undefined) affected.push(repoId);
      }
      if (affected.length) this.sourceChanged?.(affected);
      return result;
    });
  }

  private async persist(vault: Vault): Promise<void> {
    const encrypted = await this.encryption.encryptStringAsync(JSON.stringify(vault));
    await mkdir(path.dirname(this.file), { recursive: true, mode: 0o700 });
    const temporary = `${this.file}.${randomUUID()}`;
    try {
      await writeFile(temporary, encrypted, { mode: 0o600, flag: "wx" });
      await rename(temporary, this.file);
      this.cached = vault;
    } finally { await rm(temporary, { force: true }); }
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

  redactValue(value: unknown): unknown {
    if (typeof value === "string") return this.redactText(value);
    if (Array.isArray(value)) return value.map(item => this.redactValue(item));
    if (record(value)) return Object.fromEntries(Object.entries(value).map(([key, item]) => [this.redactText(key), this.redactValue(item)]));
    return value;
  }

  redactError(error: Error): Error {
    const detail = error as Error & { code?: number; data?: unknown };
    return Object.assign(new Error(this.redactText(error.message)), {
      code: detail.code, data: this.redactValue(detail.data),
    });
  }

  repositoryContext(repoId: string): { repository: string; storageConnectionId: string | null } | undefined {
    const session = this.sessions.get(repoId);
    if (!session) return;
    return { repository: session.location, storageConnectionId: this.cached?.bindings[session.key] ?? null };
  }

  async request(method: string, params: unknown, nativeSend: Send, remoteProfileId?: string): Promise<unknown> {
    const changed: IdentityChanged = async (repositories, state) => {
      try { await this.identityChanged(repositories, state); }
      catch (error) { throw error instanceof Error ? this.redactError(error) : error; }
    };
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
        // Provider failures can contain signed URLs. Strip credential material
        // at the public IPC boundary, including errors from already-open SDKs.
        throw error instanceof Error ? this.redactError(error) : error;
      } finally { if (active) this.inFlight.delete(active); }
    };
    const payload = record(params) ? { ...params } : {};
    // Renderer input never supplies the agent's private credential context.
    delete payload.__storage_auth;
    const repoId = text(payload.repo_id);
    if (remoteProfileId !== undefined && repoId && parseRepoId(repoId)?.profileId !== remoteProfileId) {
      throw new Error("The repository belongs to another SSH profile");
    }
    if (method === "storage_connections_list") {
      const vault = await this.read();
      const repoId = text(payload.repo_id);
      const selected_id = vault.bindings[this.sessions.get(repoId)?.key ?? repoId] ?? null;
      // Availability can await user input. Metadata reads must not initialize an
      // unused keyring; encrypted vault loading and mutations enforce encryption.
      return { connections: vault.connections.map(info), selected_id };
    }
    if (method === "storage_connections_save") {
      const next = connection(payload.connection);
      let retired: StorageConnection | undefined;
      try {
        const repositories = await this.mutate(vault => {
          retired = vault.connections.find(item => item.id === next.id);
          if (retired) this.inFlight.add(retired);
          vault.connections = [...vault.connections.filter(item => item.id !== next.id), next];
          return Object.keys(vault.bindings).filter(key => vault.bindings[key] === next.id);
        });
        await changed(repositories, "changed");
        return info(next);
      } finally { if (retired) this.inFlight.delete(retired); }
    }
    if (method === "storage_connections_remove") {
      const id = text(payload.id);
      let forgotten: StorageConnection | undefined;
      try {
        const bound = await this.mutate(vault => {
          forgotten = vault.connections.find(item => item.id === id);
          if (forgotten) this.inFlight.add(forgotten);
          vault.connections = vault.connections.filter(item => item.id !== id);
          // Retain bindings so an existing repo cannot silently switch identity
          // to ambient credentials after its saved connection is forgotten.
          return Object.keys(vault.bindings).filter(key => vault.bindings[key] === id);
        });
        await changed(bound, "removed");
        return { ok: true };
      } finally { if (forgotten) this.inFlight.delete(forgotten); }
    }
    if (method === "storage_connections_bind") {
      const repo = text(payload.repo_id), id = text(payload.id);
      const session = this.sessions.get(repo)?.key;
      if (!session) throw new Error("Open a repository on this device before selecting its storage connection.");
      if (!id && !(await this.read()).bindings[session]) return { ok: true };
      const repositories = await this.mutate(vault => {
        const selected = vault.connections.find(item => item.id === id);
        if (id && !selected) throw new Error("Storage connection was not found.");
        if (id) vault.bindings[session] = id; else delete vault.bindings[session];
        return [session];
      });
      // Persist before replacing native owners, including on failed reconnect.
      // Release vault admission before Agent's repository admission: opening
      // may itself persist a binding, so holding both would invert lock order.
      await changed(repositories, "changed");
      return { ok: true };
    }
    if (method === "storage_connections_test") {
      const next = payload.id
        ? (await this.read()).connections.find(item => item.id === payload.id)
        : connection(payload.connection);
      if (!next) throw new Error("Storage connection was not found.");
      return send("storage_connection_test", { repo_id: payload.repo_id, __storage_auth: next });
    }
    let session = this.sessions.get(repoId);
    const opening = method === "open_local" || method === "open_url";
    if (remoteProfileId !== undefined && opening && !repoId) throw new Error("An SSH repository identity is required");
    // Replacement resolves its destination, not the previous native owner's
    // path. Other calls retain the binding captured by their logical session.
    // Legacy physical RPCs can omit repo_id. Their remote path must never
    // select a local worktree's credentials just because the strings match.
    const source = remoteProfileId !== undefined ? repoId || undefined
      : typeof payload.path === "string" ? path.resolve(payload.path) : text(payload.cwd);
    const key = opening ? storageBindingKey(repoId, method === "open_local" ? text(payload.path) : undefined) : session?.key ?? source;
    // A desired repository can need credential repair before its first native
    // open succeeds. The binding context outlives that failed native attempt.
    if (opening && key) {
      const location = method === "open_url" ? text(payload.url) : key;
      if (session?.key !== key || session.location !== location) {
        const previous = session;
        session = { key, location, generation: 0 };
        this.sessions.set(repoId, session);
        if (previous) this.sourceChanged?.([repoId]);
      }
    }
    const queryGeneration = method === "file_query" ? session?.generation : undefined;
    const explicit = Object.hasOwn(payload, "storage_connection_id");
    const control = ["close", "file_close", "rpc/cancel"].includes(method);
    const capture = method === "files_materialize" || method === "files_dehydrate" || method === "files_restore" || method.startsWith("file_") || method.startsWith("preview_") || method.startsWith("read_");
    // An absent vault cannot bind a local repository. Avoid initializing the
    // OS key provider for plain Git browsing; initialization may await user input.
    const needsAuth = !control && (explicit || Boolean(key && this.cached?.bindings[key]) || capture || (method === "open_local" && await this.hasSavedVault() && await this.available()) || method.startsWith("crab_") || method.startsWith("workflow_") ||
      ["open_url", "clone", "push", "hydrate", "dehydrate", "storage_summary", "storage_config_get", "storage_config_verify", "git_fetch", "git_fetch_branch", "git_fetch_all", "git_pull", "git_pull_branch", "git_pull_mode", "git_push_branch", "git_push_remote", "git_remote_add", "git_remote_set_url"].includes(method));
    // Local browsing and app startup do not depend on an unlocked cloud keyring.
    const vault = needsAuth ? await this.read() : undefined;
    const id = explicit ? text(payload.storage_connection_id) : key ? vault?.bindings[key] : undefined;
    delete payload.storage_connection_id;
    let selected: StorageConnection | undefined;
    if (id) {
      selected = vault?.connections.find(item => item.id === id);
      if (!selected) throw new Error("This repository's saved connection was removed. Select another connection in Storage settings.");
    }
    // Record explicit identity before opening or cloning. If encrypted storage
    // cannot be written, do not create a cloud session or an unbound checkout.
    if (explicit && opening && key) {
      if (id || vault?.bindings[key] || await this.available()) {
        selected = await this.mutate(vault => {
          const current = id ? vault.connections.find(item => item.id === id) : undefined;
          if (id && !current) throw new Error("This repository's saved connection was removed. Select another connection in Storage settings.");
          if (id) vault.bindings[key] = id; else delete vault.bindings[key];
          return current;
        });
      }
    }
    if (method === "clone" && selected && typeof payload.dest === "string") {
      let dest = path.resolve(payload.dest);
      if (remoteProfileId !== undefined) {
        if (!path.posix.isAbsolute(payload.dest)) throw new Error("Choose an absolute remote clone destination");
        dest = `ssh://${remoteProfileId}${path.posix.normalize(payload.dest)}`;
      }
      const selectedId = selected.id;
      selected = await this.mutate(vault => {
        const current = vault.connections.find(item => item.id === selectedId);
        if (!current) throw new Error("This repository's saved connection was removed. Select another connection in Storage settings.");
        vault.bindings[dest] = selectedId;
        return current;
      });
    }
    // SQL results must belong to the admitted storage source even while the
    // native owner is waiting to retire. Fence both admission and late replies.
    const checkQuerySource = () => {
      if (method === "file_query" && (!session || this.sessions.get(repoId) !== session ||
        session.generation !== queryGeneration || session.admitted !== queryGeneration)) throw storageSourceChanged();
    };
    checkQuerySource();
    const openingGeneration = opening ? session?.generation : undefined;
    if (opening && key && !sameSource(selected, selectedConnection(this.cached, key))) throw storageSourceChanged();
    if (selected) payload.__storage_auth = selected;
    const result = await send(method, payload);
    checkQuerySource();
    if (opening && session && this.sessions.get(repoId) === session) session.admitted = openingGeneration;
    // Closing a reader is not forgetting its saved storage identity. Reopening
    // the logical snapshot must resolve the same binding after an app restart.
    if (method === "close" && this.sessions.get(repoId) === session) this.sessions.delete(repoId);
    return result;
  }
}
