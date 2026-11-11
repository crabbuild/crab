import { EventEmitter, once } from "node:events";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import path from "node:path";
import { pipeline } from "node:stream/promises";
import type { Client, ClientChannel, SFTPWrapper, Stats } from "ssh2";
import type { RemoteRuntime, RemoteTarget } from "./ssh-runtime";

export interface DeployProgress {
  stage: "detecting" | "preparing" | "uploading" | "verifying" | "starting";
  percent: number;
  speed?: number;
  eta?: number;
}

export class DeployError extends Error {
  constructor(readonly code: "WRONG_ARCH" | "RUNTIME_UNAVAILABLE" | "UPLOAD_FAILED" | "START_FAILED" | "DEPLOY_TIMED_OUT", message: string, options?: ErrorOptions) {
    super(message, options);
  }
}

type Complete<T> = (error: Error | null | undefined, value: T) => void;
const quote = (value: string) => `'${value.replace(/'/g, "'\\''")}'`;

/** Installs a verified CLI/sidecar pair without replacing another session's executable. */
export class AgentDeployer extends EventEmitter {
  constructor(private readonly resolveRuntime: (target: RemoteTarget, signal: AbortSignal) => Promise<RemoteRuntime>) { super(); }

  async ensureAgent(client: Client, signal: AbortSignal): Promise<ClientChannel> {
    this.progress("detecting", 0);
    const identity = await this.exec(client, "uname -s && uname -m", signal);
    const [os, arch, ...extra] = identity.trim().split(/\s+/);
    const platform = os === "Linux" ? "linux" : os === "Darwin" ? "darwin" : undefined;
    const architecture = arch === "x86_64" ? "x64" : arch === "aarch64" || arch === "arm64" ? "arm64" : undefined;
    if (!platform || !architecture || extra.length) throw new DeployError("WRONG_ARCH", "Remote agent requires Linux or macOS on x64 or arm64");
    const target: RemoteTarget = { platform, arch: architecture };
    this.progress("detecting", 100);
    this.progress("preparing", 0);
    let runtime: RemoteRuntime;
    try { runtime = await this.resolveRuntime(target, signal); }
    catch (cause: unknown) { signal.throwIfAborted(); throw new DeployError("RUNTIME_UNAVAILABLE", `Matching remote Crab runtime is unavailable: ${cause instanceof Error ? cause.message : "unknown failure"}`, { cause }); }
    signal.throwIfAborted(); this.progress("preparing", 100);
    const directory = await this.install(client, runtime, signal);
    this.progress("starting", 0);
    // The sidecar and every Crab/Git helper it starts use the same immutable
    // runtime. A globally installed CLI or another session cannot supply it.
    const agent = path.posix.join(directory, "crab-desktop-agent");
    const crab = path.posix.join(directory, "crab");
    const command = `exec env CRAB_BIN=${quote(crab)} PATH=${quote(directory)}:"$PATH" RUST_LOG=info ${quote(agent)}`;
    try {
      const channel = await this.wait<ClientChannel>(signal, done => client.exec(command, done), late => late.destroy());
      if (signal.aborted) { channel.destroy(); signal.throwIfAborted(); }
      // The session owner attaches its router and performs the one capability
      // handshake. A second temporary NDJSON reader could consume notifications.
      return channel;
    } catch (cause: unknown) {
      signal.throwIfAborted(); throw new DeployError("START_FAILED", "Cannot start the verified remote Crab agent", { cause });
    }
  }

  private async install(client: Client, runtime: RemoteRuntime, ownerSignal: AbortSignal): Promise<string> {
    const sftp = await this.wait<SFTPWrapper>(ownerSignal, done => client.sftp(done), late => late.end());
    const failure = new AbortController(); const signal = AbortSignal.any([ownerSignal, failure.signal]);
    const failedSubsystem = (error: Error) => failure.abort(error);
    sftp.on("error", failedSubsystem);
    const sftpClosed = new Promise<void>(resolve => sftp.once("close", () => { sftp.removeListener("error", failedSubsystem); resolve(); }));
    let failed = false;
    let lease: { channel: ClientChannel; closed: Promise<void> } | undefined;
    try {
      signal.throwIfAborted();
      const home = await this.wait<string>(signal, done => sftp.realpath(".", done));
      if (!path.posix.isAbsolute(home)) throw new Error("SFTP home directory is not absolute");
      let parent = home;
      for (const component of [".crab", "desktop", "runtimes", runtime.version, `${runtime.platform}-${runtime.arch}`]) {
        parent = path.posix.join(parent, component); await this.directory(sftp, parent, signal);
      }
      const digest = createHash("sha256").update(`${runtime.hashes.agent}\n${runtime.hashes.crab}`).digest("hex");
      const destination = path.posix.join(parent, digest);
      const existing = await this.info(sftp, destination, signal);
      if (existing) {
        if (!existing.isDirectory()) throw new Error("Remote runtime path is not a regular directory");
        await this.verify(client, sftp, destination, runtime, signal);
        return destination;
      }
      const temporary = path.posix.join(parent, `.upload-${randomUUID()}`);
      lease = await this.stagingLease(client, temporary, signal);
      this.progress("uploading", 0);
      const total = (await stat(runtime.agent)).size + (await stat(runtime.crab)).size;
      let transferred = 0; let reported = 0; const started = Date.now();
      const report = (length: number) => {
        transferred += length;
        const seconds = (Date.now() - started) / 1000; const speed = seconds > 0 ? transferred / seconds : 0;
        const percent = Math.min(100, Math.round(transferred * 100 / total)); const eta = speed > 0 ? Math.max(0, total - transferred) / speed : 0;
        if (percent !== reported) { reported = percent; this.progress("uploading", percent, speed, eta); }
      };
      for (const [local, name] of [[runtime.agent, "crab-desktop-agent"], [runtime.crab, "crab"]]) {
        const remote = path.posix.join(temporary, name);
        await this.upload(sftp, local, remote, signal, report);
      }
      const cli = path.posix.join(temporary, "crab"); const helper = path.posix.join(temporary, "git-remote-crab");
      await this.exec(client, `ln ${quote(cli)} ${quote(helper)}`, signal);
      await this.verify(client, sftp, temporary, runtime, signal);
      try { await this.wait<void>(signal, done => sftp.rename(temporary, destination, error => done(error, undefined))); }
      catch (cause: unknown) {
        // SFTP v3 does not replace an existing directory. A concurrent owner may
        // win publication; only its complete, matching runtime is reusable.
        if (!await this.info(sftp, destination, signal)) throw cause;
        await this.verify(client, sftp, destination, runtime, signal);
        return destination;
      }
      return destination;
    } catch (cause: unknown) {
      failed = true;
      ownerSignal.throwIfAborted();
      if (cause instanceof DeployError) throw cause;
      throw new DeployError("UPLOAD_FAILED", `Remote Crab runtime installation failed: ${cause instanceof Error ? cause.message : "unknown failure"}`, { cause });
    } finally {
      // Retire SFTP before releasing the guard so cleanup cannot race a pending
      // remote OPEN/WRITE. Published runtimes are outside its cleanup scope.
      sftp.end();
      try {
        await this.wait<void>(AbortSignal.timeout(5000), done => { void sftpClosed.then(() => done(undefined, undefined)); });
        if (lease) {
          lease.channel.end();
          const closed = lease.closed;
          await this.wait<void>(AbortSignal.timeout(5000), done => { void closed.then(() => done(undefined, undefined)); });
        }
      } catch (cause: unknown) {
        lease?.channel.end(); lease?.channel.destroy();
        if (!failed && !signal.aborted) throw new DeployError("UPLOAD_FAILED", "Remote upload guard did not close", { cause });
      } finally { sftp.end(); }
    }
  }

  private async upload(sftp: SFTPWrapper, local: string, remote: string, signal: AbortSignal, report: (bytes: number) => void): Promise<void> {
    const idle = new AbortController(); const transferSignal = AbortSignal.any([signal, idle.signal]);
    let timer: NodeJS.Timeout | undefined;
    const touch = () => { clearTimeout(timer); timer = setTimeout(() => idle.abort(new DeployError("DEPLOY_TIMED_OUT", "Remote upload made no progress for 30s")), 30_000); };
    const source = createReadStream(local); const sourceClosed = Promise.allSettled([once(source, "close")]);
    let destination: ReturnType<SFTPWrapper["createWriteStream"]> | undefined; let cancel: (() => void) | undefined;
    const data = (chunk: Buffer | string) => { touch(); report(typeof chunk === "string" ? Buffer.byteLength(chunk) : chunk.length); };
    try {
      touch(); source.on("data", data);
      destination = sftp.createWriteStream(remote, { flags: "wx", mode: 0o700 }); destination.on("drain", touch);
      // Writable finish precedes the SFTP CLOSE reply. Both acknowledgements
      // are required before checksum verification and publication.
      const closed = once(destination, "close"); const copying = pipeline(source, destination, { signal: transferSignal });
      const copied = Promise.all([copying, closed]);
      const cancelled = new Promise<never>((_resolve, reject) => {
        cancel = () => reject(transferSignal.reason); transferSignal.addEventListener("abort", cancel, { once: true });
        if (transferSignal.aborted) cancel();
      });
      // SSH2 can still be waiting for a remote OPEN/CLOSE callback after stream
      // destruction. The owner retires that SFTP session when this wait aborts.
      await Promise.race([copied, cancelled]);
    } finally {
      clearTimeout(timer); if (cancel) transferSignal.removeEventListener("abort", cancel);
      source.removeListener("data", data); destination?.removeListener("drain", touch);
      source.destroy(); destination?.destroy(); await sourceClosed;
    }
  }

  private async stagingLease(client: Client, directory: string, signal: AbortSignal): Promise<{ channel: ClientChannel; closed: Promise<void> }> {
    const files = ["crab-desktop-agent", "crab", "git-remote-crab"].map(name => quote(path.posix.join(directory, name))).join(" ");
    const cleanup = `if [ -d ${quote(directory)} ]; then rm -f ${files}; rmdir ${quote(directory)}; fi`;
    // Register cleanup before creating anything. SSH stdin EOF or termination
    // retires only this unpredictable upload directory, including on disconnect.
    const script = `cleanup() { ${cleanup}; }; trap cleanup EXIT; trap 'exit 1' HUP INT TERM; umask 077; mkdir ${quote(directory)} || exit 1; printf 'ready\\n'; cat >/dev/null`;
    const command = `sh -c ${quote(script)}`;
    const channel = await this.wait<ClientChannel>(signal, done => client.exec(command, done), late => late.destroy());
    const consume = () => {};
    channel.on("error", consume);
    const closed = new Promise<void>(resolve => channel.once("close", () => { channel.removeListener("error", consume); resolve(); }));
    let received = "";
    let ready: ((data: Buffer) => void) | undefined; let ended: (() => void) | undefined;
    try {
      await this.wait<void>(signal, done => {
        ready = data => {
          received += data.toString();
          if (received === "ready\n") done(undefined, undefined);
          else if (received.length >= 6) done(new Error("Remote upload guard did not acknowledge readiness"), undefined);
        };
        ended = () => done(new Error("Remote upload guard closed before readiness"), undefined);
        channel.on("data", ready); channel.once("close", ended);
      });
      return { channel, closed };
    } catch (cause: unknown) { channel.destroy(); throw cause; }
    finally { if (ready) channel.removeListener("data", ready); if (ended) channel.removeListener("close", ended); channel.stderr.resume(); }
  }

  private async directory(sftp: SFTPWrapper, directory: string, signal: AbortSignal): Promise<void> {
    let info = await this.info(sftp, directory, signal);
    if (!info) {
      try { await this.wait<void>(signal, done => sftp.mkdir(directory, { mode: 0o700 }, error => done(error, undefined))); }
      catch (cause: unknown) { if (!await this.info(sftp, directory, signal)) throw cause; }
      info = await this.info(sftp, directory, signal);
    }
    if (!info?.isDirectory()) throw new Error("Remote runtime parent is not a regular directory");
  }

  private async info(sftp: SFTPWrapper, file: string, signal: AbortSignal): Promise<Stats | undefined> {
    try { return await this.wait<Stats>(signal, done => sftp.lstat(file, done)); }
    catch (cause: unknown) {
      if ((cause as { code?: number }).code === 2) return undefined;
      throw cause;
    }
  }

  private async verify(client: Client, sftp: SFTPWrapper, directory: string, runtime: RemoteRuntime, signal: AbortSignal): Promise<void> {
    this.progress("verifying", 0);
    const checksum = runtime.platform === "darwin" ? "shasum -a 256" : "sha256sum";
    for (const [name, expected] of [["crab-desktop-agent", runtime.hashes.agent], ["crab", runtime.hashes.crab], ["git-remote-crab", runtime.hashes.crab]]) {
      const file = path.posix.join(directory, name); const info = await this.info(sftp, file, signal);
      if (!info?.isFile() || !(info.mode & 0o100)) throw new Error(`Remote runtime binary is not a regular executable: ${name}`);
      // Hash stdin so spaces, Unicode and newlines in the remote home path do
      // not become checksum-tool filename escapes or extra output records.
      const output = await this.exec(client, `${checksum} < ${quote(file)}`, signal);
      const actual = output.trim().match(/^([a-f0-9]{64})[ \t]+-$/)?.[1];
      if (actual !== expected) throw new Error(`Remote runtime checksum mismatch: ${name}`);
    }
    this.progress("verifying", 100);
  }

  private wait<T>(signal: AbortSignal, begin: (done: Complete<T>) => void, retire?: (value: T) => void): Promise<T> {
    return new Promise((resolve, reject) => {
      let settled = false;
      const cleanup = () => { settled = true; clearTimeout(timer); signal.removeEventListener("abort", cancel); };
      const cancel = () => { cleanup(); reject(signal.reason); };
      const timer = setTimeout(() => { cleanup(); reject(new DeployError("DEPLOY_TIMED_OUT", "Remote deployment operation timed out")); }, 15_000);
      signal.addEventListener("abort", cancel, { once: true });
      if (signal.aborted) { cancel(); return; }
      try { begin((error, value) => {
        if (settled) { if (!error) retire?.(value); return; }
        cleanup(); if (error) reject(error); else resolve(value);
      }); } catch (cause: unknown) { cleanup(); reject(cause); }
    });
  }

  private async exec(client: Client, command: string, signal: AbortSignal): Promise<string> {
    const channel = await this.wait<ClientChannel>(signal, done => client.exec(command, done), late => late.destroy());
    try {
      return await this.wait<string>(signal, done => {
        let stdout = ""; let stderr = ""; let received = 0;
        const collect = (data: Buffer, diagnostic: boolean) => {
          received += data.length;
          if (received > 64 * 1024) { done(new Error("Remote deployment command output exceeded 64 KiB"), ""); channel.destroy(); return; }
          if (diagnostic) stderr += data.toString(); else stdout += data.toString();
        };
        channel.on("data", (data: Buffer) => collect(data, false)); channel.stderr.on("data", (data: Buffer) => collect(data, true));
        channel.on("error", (error: Error) => done(error, ""));
        // Unlike a connectivity probe, publishing an executable needs a positive
        // exit status. An optional SSH status cannot confirm a filesystem action.
        channel.once("close", (code?: number | null) => code === 0 ? done(undefined, stdout) : done(new Error(`Remote deployment command failed (exit ${code ?? "unreported"}): ${stderr.trim()}`), ""));
      });
    } finally { channel.destroy(); }
  }

  private progress(stage: DeployProgress["stage"], percent: number, speed?: number, eta?: number): void {
    this.emit("ssh:deploy-progress", { stage, percent, speed, eta } satisfies DeployProgress);
  }
}
