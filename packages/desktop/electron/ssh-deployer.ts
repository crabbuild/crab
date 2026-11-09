import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, readFile } from "node:fs/promises";
import * as path from "node:path";
import type { Client, ClientChannel, SFTPWrapper } from "ssh2";

export interface DeployProgress {
  stage: "detecting" | "uploading" | "verifying" | "starting";
  percent: number;
  speed?: number;
  eta?: number;
}

export const SSH_RUNTIME_TARGETS = ["darwin-x64", "darwin-arm64", "linux-x64", "linux-arm64"] as const;

type DeployErrorCode = "RUNTIME_MISSING" | "RUNTIME_INVALID" | "WRONG_ARCH" | "UPLOAD_FAILED" | "START_FAILED";

export class DeployError extends Error {
  constructor(public readonly code: DeployErrorCode, message: string, public readonly suggestion?: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "DeployError";
  }
}

interface Runtime {
  version: string;
  platform: "linux" | "darwin";
  arch: "x64" | "arm64";
  hashes: Record<"crab" | "crab-desktop-agent", string>;
  archive: { sha256: string; bytes: number };
}

async function digest(file: string, signal?: AbortSignal): Promise<string> {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file, { signal })) hash.update(chunk);
  return hash.digest("hex");
}

export async function readSshRuntime(directory: string, target: string, version: string, signal?: AbortSignal): Promise<Runtime & { file: string; id: string }> {
  if (!SSH_RUNTIME_TARGETS.some(value => value === target)) throw new DeployError("WRONG_ARCH", `Unsupported SSH runtime target: ${target}`);
  let value: unknown;
  try { value = JSON.parse(await readFile(path.join(directory, `${target}.json`), { encoding: "utf8", signal })); }
  catch (cause) {
    if (signal?.aborted) throw cause;
    const code = (cause as NodeJS.ErrnoException).code === "ENOENT" ? "RUNTIME_MISSING" : "RUNTIME_INVALID";
    throw new DeployError(code, `Cannot read the bundled ${target} SSH runtime`, undefined, { cause });
  }
  const runtime = value as Partial<Runtime> | null;
  const validHash = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
  if (!runtime || runtime.version !== version || `${runtime.platform}-${runtime.arch}` !== target ||
      !validHash(runtime.hashes?.crab) || !validHash(runtime.hashes?.["crab-desktop-agent"]) ||
      Object.keys(runtime.hashes).sort().join(",") !== "crab,crab-desktop-agent" ||
      !validHash(runtime.archive?.sha256) || !Number.isSafeInteger(runtime.archive.bytes) || runtime.archive.bytes <= 0) {
    throw new DeployError("RUNTIME_INVALID", `Bundled ${target} SSH runtime does not match this application`);
  }
  const file = path.join(directory, `${target}.tar.gz`);
  const stat = await lstat(file);
  if (!stat.isFile() || stat.size !== runtime.archive.bytes || await digest(file, signal) !== runtime.archive.sha256) {
    throw new DeployError("RUNTIME_INVALID", `Bundled ${target} SSH runtime archive failed checksum verification`);
  }
  const complete = runtime as Runtime;
  // Archive timestamps can vary between builds. Installation identity belongs
  // to the executable pair, so packaging it again reuses the same runtime.
  const id = createHash("sha256").update(`${complete.hashes.crab}\n${complete.hashes["crab-desktop-agent"]}\n`).digest("hex");
  return { ...complete, file, id };
}

function quote(value: string): string { return `'${value.replaceAll("'", "'\\''")}'`; }

export class AgentDeployer {
  constructor(private readonly runtimesDirectory: string, private readonly version: string) {}

  async ensureAgent(client: Client, progress: (value: DeployProgress) => void = () => {}, signal?: AbortSignal): Promise<ClientChannel> {
    signal?.throwIfAborted();
    const exec = (command: string) => this.exec(client, command, signal);
    progress({ stage: "detecting", percent: 0 });
    const detected = (await exec("uname -s && uname -m")).trim().split(/\r?\n/);
    const platform = detected[0] === "Linux" ? "linux" : detected[0] === "Darwin" ? "darwin" : null;
    const arch = detected[1] === "x86_64" ? "x64" : ["aarch64", "arm64"].includes(detected[1]) ? "arm64" : null;
    if (detected.length !== 2 || !platform || !arch) throw new DeployError("WRONG_ARCH", `Unsupported remote platform: ${detected.join(" ")}`);
    progress({ stage: "detecting", percent: 100 });
    progress({ stage: "verifying", percent: 0 });
    const runtime = await readSshRuntime(this.runtimesDirectory, `${platform}-${arch}`, this.version, signal);
    const sftp = await new Promise<SFTPWrapper>((resolve, reject) => client.sftp((error, stream) => error ? reject(error) : resolve(stream)));
    let installed: string;
    const abortUpload = () => sftp.end();
    signal?.addEventListener("abort", abortUpload, { once: true });
    try {
      signal?.throwIfAborted();
      const remoteHome = await new Promise<string>((resolve, reject) => sftp.realpath(".", (error, result) => error ? reject(error) : resolve(result)));
      if (!path.posix.isAbsolute(remoteHome) || remoteHome.includes("\0")) throw new DeployError("UPLOAD_FAILED", "SSH server returned an invalid home directory");
      const root = path.posix.join(remoteHome, ".crab", "desktop", "runtimes");
      installed = path.posix.join(root, runtime.id);
      const existing = await exec(`if test -d ${quote(installed)}; then printf installed; fi`);
      if (existing === "installed") {
        await this.verifyPair(client, installed, runtime, signal);
      } else if (existing === "") {
        const temporary = path.posix.join(root, `.upload-${randomUUID()}`);
        await exec(`mkdir -p ${quote(root)} && mkdir ${quote(temporary)}`);
        let failure: unknown;
        try {
          const archive = path.posix.join(temporary, "payload.tar.gz");
          const started = Date.now();
          progress({ stage: "uploading", percent: 0 });
          await new Promise<void>((resolve, reject) => {
            sftp.fastPut(runtime.file, archive, { step: transferred => {
              const speed = transferred / Math.max((Date.now() - started) / 1000, 0.001);
              progress({ stage: "uploading", percent: transferred / runtime.archive.bytes * 100, speed, eta: (runtime.archive.bytes - transferred) / speed });
            } }, error => error ? reject(error) : resolve());
          });
          await this.verifyHash(client, archive, runtime.archive.sha256, platform, signal);
          await exec(`tar -xzf ${quote(archive)} -C ${quote(temporary)} && rm ${quote(archive)} && chmod 700 ${quote(path.posix.join(temporary, "crab"))} ${quote(path.posix.join(temporary, "crab-desktop-agent"))} && ln -s crab ${quote(path.posix.join(temporary, "git-remote-crab"))}`);
          await this.verifyPair(client, temporary, runtime, signal);
          try {
            await new Promise<void>((resolve, reject) => sftp.rename(temporary, installed, error => error ? reject(error) : resolve()));
          } catch (cause) {
            // Another application can install the same immutable pair first.
            // Reuse it only after verification; never overwrite a live runtime.
            try { await this.verifyPair(client, installed, runtime, signal); }
            catch { throw new DeployError("UPLOAD_FAILED", "Cannot publish the verified SSH runtime", undefined, { cause }); }
          }
        } catch (error) { failure = error; }
        try { await this.exec(client, `rm -rf -- ${quote(temporary)}`); }
        catch (cleanup) { failure = failure ? new AggregateError([failure, cleanup], "SSH runtime upload and cleanup failed") : cleanup; }
        if (failure) throw failure;
      } else throw new DeployError("RUNTIME_INVALID", "Unexpected remote runtime installation response");
    } finally { signal?.removeEventListener("abort", abortUpload); sftp.end(); }
    signal?.throwIfAborted();
    progress({ stage: "verifying", percent: 100 });
    progress({ stage: "starting", percent: 0 });
    const index = path.posix.join(path.posix.dirname(installed), `.index-${randomUUID()}`);
    // EOF drains the native process before the shell removes its unique index.
    // Parallel profiles must never share the default SlateDB writer directory.
    const command = `crab_index=${quote(index)}; mkdir -p "$crab_index" || exit; trap 'rm -rf -- "$crab_index"' EXIT; CRAB_BIN=${quote(path.posix.join(installed, "crab"))} PATH=${quote(installed)}:"$PATH" ${quote(path.posix.join(installed, "crab-desktop-agent"))} --index-dir "$crab_index"`;
    const channel = await new Promise<ClientChannel>((resolve, reject) => client.exec(command, (cause, stream) => {
      if (cause) reject(new DeployError("START_FAILED", "Cannot start the verified SSH runtime", undefined, { cause }));
      else resolve(stream);
    }));
    progress({ stage: "starting", percent: 100 });
    return channel;
  }

  private async verifyPair(client: Client, directory: string, runtime: Runtime, signal?: AbortSignal): Promise<void> {
    await Promise.all(Object.entries(runtime.hashes).map(([name, expected]) => this.verifyHash(client, path.posix.join(directory, name), expected, runtime.platform, signal)));
  }

  private async verifyHash(client: Client, file: string, expected: string, platform: Runtime["platform"], signal?: AbortSignal): Promise<void> {
    const command = platform === "linux" ? "sha256sum" : "shasum -a 256";
    const output = await this.exec(client, `${command} ${quote(file)}`, signal);
    if (output.slice(0, 64) !== expected || !/^\s/.test(output.slice(64))) {
      throw new DeployError("RUNTIME_INVALID", "Remote SSH runtime failed checksum verification");
    }
  }

  private exec(client: Client, command: string, signal?: AbortSignal): Promise<string> {
    return new Promise((resolve, reject) => {
      if (signal?.aborted) { reject(signal.reason); return; }
      client.exec(command, (cause, stream) => {
        if (cause) { reject(cause); return; }
        const stdout: Buffer[] = [], stderr: Buffer[] = [];
        let bytes = 0, failure: Error | undefined;
        const stop = (error: Error) => {
          if (failure) return;
          failure = error;
          stream.signal("TERM"); stream.end();
        };
        // A timed-out control command still owns its remote process until the
        // server closes the channel. Never report its cleanup as completed early.
        const timer = setTimeout(() => stop(new Error("SSH runtime control command timed out")), 30_000);
        const collect = (chunks: Buffer[], data: Buffer) => {
          bytes += data.length;
          if (bytes > 64 * 1024) stop(new Error("SSH runtime control output exceeds 64 KiB"));
          else chunks.push(data);
        };
        stream.on("data", (data: Buffer) => collect(stdout, data));
        stream.stderr.on("data", (data: Buffer) => collect(stderr, data));
        const aborted = () => stop(signal?.reason instanceof Error ? signal.reason : new Error("SSH runtime deployment cancelled"));
        signal?.addEventListener("abort", aborted, { once: true });
        if (signal?.aborted) aborted();
        stream.on("error", (error: Error) => { failure ??= error; });
        stream.once("close", (code: number | undefined) => {
          clearTimeout(timer);
          signal?.removeEventListener("abort", aborted);
          if (failure) reject(failure);
          else if (code !== 0) reject(new DeployError("UPLOAD_FAILED", `SSH runtime command failed (${code}): ${Buffer.concat(stderr).toString("utf8")}`));
          else resolve(Buffer.concat(stdout).toString("utf8"));
        });
        stream.end();
      });
    });
  }
}
