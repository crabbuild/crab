import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, mkdir, open, readFile, rename, rm } from "node:fs/promises";
import path from "node:path";

export interface RemoteTarget { platform: "darwin" | "linux"; arch: "arm64" | "x64" }
export interface RemoteRuntime extends RemoteTarget {
  version: string;
  agent: string;
  crab: string;
  hashes: { agent: string; crab: string };
}
interface RuntimeStoreOptions { packaged: boolean; appPath: string; version: string; cacheDirectory: string }

const VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-(alpha|beta|rc)\.(0|[1-9]\d*))?$/;
const SHA256 = /^[a-f0-9]{64}$/;
const RELEASE_HOSTS = new Set(["github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com"]);
const FILES = { agent: "crab-desktop-agent", crab: "crab" } as const;

function manifest(value: unknown, version: string, target: RemoteTarget): RemoteRuntime["hashes"] {
  if (!value || typeof value !== "object") throw new Error("Invalid remote runtime manifest");
  const record = value as Record<string, unknown>;
  if (record.version !== version || record.platform !== target.platform || record.arch !== target.arch || !record.hashes || typeof record.hashes !== "object") {
    throw new Error("Remote runtime does not match this application's version and the server target");
  }
  const hashes = record.hashes as Record<string, unknown>;
  const agent = hashes[FILES.agent]; const crab = hashes[FILES.crab];
  if (typeof agent !== "string" || !SHA256.test(agent) || typeof crab !== "string" || !SHA256.test(crab)) throw new Error("Invalid remote runtime checksums");
  return { agent, crab };
}

async function hash(file: string, signal: AbortSignal): Promise<string> {
  const stat = await lstat(file);
  if (!stat.isFile() || stat.size === 0) throw new Error(`Runtime binary must be a nonempty regular file: ${file}`);
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(file, { signal })) digest.update(chunk);
  return digest.digest("hex");
}

/** Resolves an exact release runtime for SSH without mixing binary versions or targets. */
export class RemoteRuntimeStore {
  private readonly pending = new Map<string, { controller: AbortController; promise: Promise<RemoteRuntime>; borrowers: number }>();
  constructor(private readonly options: RuntimeStoreOptions, private readonly request: typeof fetch = fetch) {
    if (!VERSION.test(options.version)) throw new Error("Remote deployment requires a release version");
  }

  async resolve(target: RemoteTarget, signal: AbortSignal): Promise<RemoteRuntime> {
    signal.throwIfAborted();
    const key = `${target.platform}-${target.arch}`;
    let entry = this.pending.get(key);
    if (!entry) {
      const controller = new AbortController();
      const promise = this.load(target, controller.signal);
      const owner = { controller, promise, borrowers: 0 }; entry = owner; this.pending.set(key, owner);
      const retire = () => { if (this.pending.get(key) === owner) this.pending.delete(key); };
      void promise.then(retire, retire);
    }
    const owner = entry; owner.borrowers += 1;
    let cancel!: () => void;
    const cancelled = new Promise<never>((_resolve, reject) => {
      cancel = () => reject(signal.reason); signal.addEventListener("abort", cancel, { once: true });
      if (signal.aborted) cancel();
    });
    try {
      const runtime = await Promise.race([owner.promise, cancelled]); signal.throwIfAborted(); return runtime;
    } finally {
      signal.removeEventListener("abort", cancel); owner.borrowers -= 1;
      // One profile cannot cancel another profile's shared artifact. The last
      // borrower owns cancellation and waits for its private download files to retire.
      if (!owner.borrowers && this.pending.get(key) === owner) {
        this.pending.delete(key); owner.controller.abort(signal.reason);
        await owner.promise.catch(() => {});
      }
    }
  }

  private async load(target: RemoteTarget, signal: AbortSignal): Promise<RemoteRuntime> {
    signal.throwIfAborted();
    const { options } = this;
    if (!options.packaged) {
      const directory = path.join(options.appPath, "resources", "native", `${target.platform === "darwin" ? "mac" : "linux"}-${target.arch}`);
      return this.verify(directory, target, signal);
    }
    const directory = path.join(options.cacheDirectory, options.version, `${target.platform}-${target.arch}`);
    let cached = false;
    try { await lstat(directory); cached = true; }
    catch (cause: unknown) { if ((cause as NodeJS.ErrnoException).code !== "ENOENT") throw cause; }
    if (cached) {
      try { return await this.verify(directory, target, signal); }
      catch (cause: unknown) {
        signal.throwIfAborted();
        const reason = cause instanceof Error ? cause.message : "invalid files";
        throw new Error(`Remote runtime cache is invalid: ${reason}. Remove ${directory} and reconnect.`, { cause });
      }
    }
    await mkdir(path.dirname(directory), { recursive: true, mode: 0o700 });
    const temporary = `${directory}-${randomUUID()}`;
    await mkdir(temporary, { mode: 0o700 });
    try {
      const prefix = `Crab-${options.version}-runtime-${target.platform}-${target.arch}`;
      const base = `https://github.com/crabbuild/crab/releases/download/v${options.version}/${prefix}`;
      const downloadSignal = AbortSignal.any([signal, AbortSignal.timeout(5 * 60_000)]);
      await this.download(`${base}.json`, path.join(temporary, "runtime.json"), downloadSignal, 64 * 1024);
      const hashes = manifest(JSON.parse(await readFile(path.join(temporary, "runtime.json"), "utf8")), options.version, target);
      for (const key of ["agent", "crab"] as const) {
        const actual = await this.download(`${base}-${FILES[key]}`, path.join(temporary, FILES[key]), downloadSignal, 512 * 1024 * 1024);
        if (actual !== hashes[key]) throw new Error(`Remote runtime checksum mismatch: ${FILES[key]}`);
      }
      signal.throwIfAborted();
      try { await rename(temporary, directory); }
      catch (cause: unknown) {
        if (!["EEXIST", "ENOTEMPTY"].includes((cause as NodeJS.ErrnoException).code ?? "")) throw cause;
        // Concurrent connections may publish the same release cache. Only a
        // fully verified winner can replace this attempt's completed download.
      }
      return await this.verify(directory, target, signal);
    } finally { await rm(temporary, { recursive: true, force: true }); }
  }

  private async verify(directory: string, target: RemoteTarget, signal: AbortSignal): Promise<RemoteRuntime> {
    const stat = await lstat(directory);
    if (!stat.isDirectory() || stat.isSymbolicLink()) throw new Error("Remote runtime directory must not be a symbolic link");
    const metadata = path.join(directory, "runtime.json");
    const metadataStat = await lstat(metadata);
    if (!metadataStat.isFile() || metadataStat.size > 64 * 1024) throw new Error("Invalid remote runtime manifest file");
    const hashes = manifest(JSON.parse(await readFile(metadata, "utf8")), this.options.version, target);
    const agent = path.join(directory, FILES.agent); const crab = path.join(directory, FILES.crab);
    for (const [file, expected] of [[agent, hashes.agent], [crab, hashes.crab]]) {
      if (await hash(file, signal) !== expected) throw new Error(`Remote runtime checksum mismatch: ${path.basename(file)}`);
    }
    signal.throwIfAborted();
    return { ...target, version: this.options.version, agent, crab, hashes };
  }

  private async download(url: string, destination: string, signal: AbortSignal, limit: number): Promise<string> {
    let current = new URL(url);
    let response: Response | undefined;
    for (let redirects = 0; redirects <= 5; redirects++) {
      if (current.protocol !== "https:" || !RELEASE_HOSTS.has(current.hostname) || current.username || current.password) throw new Error("Remote runtime download left the official release hosts");
      signal.throwIfAborted();
      response = await this.request(current, { signal, redirect: "manual" });
      if (![301, 302, 303, 307, 308].includes(response.status)) break;
      const location = response.headers.get("location");
      await response.body?.cancel();
      if (!location || redirects === 5) throw new Error("Invalid remote runtime release redirect");
      current = new URL(location, current);
    }
    if (!response?.ok || !response.body) {
      await response?.body?.cancel();
      throw new Error(`Remote runtime release download failed (HTTP ${response?.status ?? "unknown"})`);
    }
    const reader = response.body.getReader();
    let file: Awaited<ReturnType<typeof open>> | undefined;
    let completed = false;
    const digest = createHash("sha256"); let received = 0;
    try {
      file = await open(destination, "wx", 0o600);
      while (true) {
        signal.throwIfAborted(); const chunk = await reader.read();
        if (chunk.done) break;
        received += chunk.value.byteLength;
        if (received > limit) throw new Error("Remote runtime release asset exceeds its size limit");
        digest.update(chunk.value); await file.writeFile(chunk.value);
      }
      if (!received) throw new Error("Remote runtime release asset is empty");
      await file.sync(); completed = true;
      return digest.digest("hex");
    } finally {
      const cleanup = await Promise.allSettled([reader.cancel(), file?.close()]);
      const errors = cleanup.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (completed && errors.length) throw new AggregateError(errors.map(error => error.reason), "Cannot close remote runtime download");
    }
  }
}
