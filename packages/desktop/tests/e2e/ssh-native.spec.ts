import { test, expect } from "@playwright/test";
import { spawn, execFileSync, type ChildProcessWithoutNullStreams } from "node:child_process";
import { generateKeyPairSync, randomUUID, timingSafeEqual } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { Client, Server, utils, type ClientChannel } from "ssh2";
import type { CapabilityNegotiator } from "../../electron/ssh-capabilities";
import type { RpcRouter } from "../../electron/ssh-router";
import type { SshSessionManager } from "../../electron/ssh-manager";
import { packagedExecutable, packagedModule, unboundStorage } from "./packaged-app";

function running(pid: number): boolean {
  try { process.kill(pid, 0); return true; }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; }
}
function descendants(parent: number): number[] {
  return execFileSync("ps", ["-axo", "pid=,ppid=,comm="], { encoding: "utf8" }).trim().split("\n")
    .map(line => /^\s*(\d+)\s+(\d+)\s+(.+)$/.exec(line))
    .filter((entry): entry is RegExpExecArray => !!entry && Number(entry[2]) === parent && path.basename(entry[3]) === "crab")
    .map(entry => Number(entry[1]));
}
async function cleanExit(child: ChildProcessWithoutNullStreams): Promise<void> {
  const exit = child.exitCode === null && child.signalCode === null ? once(child, "exit") : Promise.resolve([child.exitCode, child.signalCode]);
  child.stdin.end();
  let guard: ReturnType<typeof setTimeout> | undefined;
  try {
    const result = await Promise.race([exit, new Promise<never>((_, reject) => {
      guard = setTimeout(() => { child.kill("SIGKILL"); reject(new Error("Native child failed to drain within 15 seconds")); }, 15_000);
    })]);
    expect(result).toEqual([0, null]);
  } finally { clearTimeout(guard); }
}

test("packaged SSH reader drains cancelled staging admission and materializes exact Crab/LFS bytes", async () => {
  test.skip(process.platform === "win32", "Exclusive staging lock fixture uses POSIX flock; Windows qualification remains separate");
  test.setTimeout(120_000);
  const executable = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executable), "../Resources") : path.join(path.dirname(executable), "resources");
  const crab = path.join(resources, "agent/crab");
  const agent = path.join(resources, "agent/crab-desktop-agent");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-native-"));
  const source = path.join(root, "source"); mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: crab, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  const { CapabilityNegotiator: PackagedNegotiator } = packagedModule<{ CapabilityNegotiator: typeof CapabilityNegotiator }>(resources, "ssh-capabilities");
  const { RpcRouter: PackagedRouter } = packagedModule<{ RpcRouter: typeof RpcRouter }>(resources, "ssh-router");
  const key = generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs1", format: "pem" }, publicKeyEncoding: { type: "pkcs1", format: "pem" } });
  const host = utils.parseKey(key.privateKey);
  if (host instanceof Error) throw host;
  const hostPublic = host.getPublicSSH();
  const password = randomUUID();
  const clients: Client[] = [];
  const children: ChildProcessWithoutNullStreams[] = [];
  const transportErrors: Error[] = [];
  let splitUtf8Observed = false;
  let writer: ChildProcessWithoutNullStreams | undefined;
  let server: Server | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    mkdirSync(path.join(source, ".crab"));
    writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks"))}\n`);
    const driver = `'${crab.replaceAll("'", "'\\''")}' filter-process`;
    for (const kind of ["crab", "lfs"]) {
      git(["config", `filter.${kind}.process`, driver]); git(["config", `filter.${kind}.required`, "true"]);
    }
    writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
    writeFileSync(path.join(source, ".gitattributes"), "dataset.bin filter=crab -text\nlfs.bin filter=lfs -text\n");
    const content = Buffer.alloc(4 * 1024 ** 2);
    for (let i = 0; i < content.length; i++) content[i] = (i * 37 + (i >>> 12)) & 255;
    const pointers = new Map<string, Buffer>();
    for (const file of ["dataset.bin", "lfs.bin"]) {
      writeFileSync(path.join(source, file), content); git(["add", file]);
      const pointer = git(["show", `:${file}`]); expect(pointer.length).toBeLessThan(1024);
      pointers.set(file, pointer); writeFileSync(path.join(source, file), pointer);
    }
    const text = "こんにちは 🦀 中文\n";
    writeFileSync(path.join(source, "unicode.txt"), text); git(["add", ".gitattributes", ".gitignore", "unicode.txt"]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "SSH captured source fixture"]);
    server = new Server({ hostKeys: [key.privateKey] }, connection => {
      connection.on("error", error => transportErrors.push(error));
      connection.on("authentication", context => {
        if (context.username === "crab-test" && context.method === "password" && context.password === password) context.accept();
        else context.reject();
      });
      connection.on("ready", () => connection.on("session", accept => {
        const session = accept();
        session.once("exec", (accept, reject, info) => {
          if (info.command !== "crab-desktop-agent") { reject(); return; }
          const stream = accept();
          const child = spawn(agent, ["--index-dir", path.join(root, `index-${children.length}`)], { cwd: source, env, stdio: "pipe" });
          children.push(child);
          let stderr = "";
          child.stderr.on("data", data => { stderr = (stderr + data.toString()).slice(-8192); });
          stream.pipe(child.stdin);
          child.stdout.on("data", (bytes: Buffer) => {
            // Fragment actual SSH packets inside Unicode codepoints. Large
            // base64 frames avoid millions of individual one-byte writes.
            if (bytes.length < 4096) for (const byte of bytes) stream.write(Buffer.from([byte]));
            else stream.write(bytes);
          });
          child.once("error", error => transportErrors.push(error));
          child.once("close", (code, signal) => {
            if (code !== 0 || signal) transportErrors.push(new Error(`Native agent exit ${code}/${signal}: ${stderr}`));
            stream.exit(code ?? 1); stream.end();
          });
        });
      }));
    });
    server.on("error", (error: Error) => transportErrors.push(error)); server.listen(0, "127.0.0.1"); await once(server, "listening");
    const address = server.address();
    if (typeof address !== "object" || !address) throw new Error("Missing SSH listener address");
    const channels = new Map<string, ClientChannel>();
    const profiles = new Map<string, { operationTimeoutMs?: number }>();
    const statuses = new Map<string, string>();
    const manager = { repositoryWork: () => undefined, getStatus: (id: string) => statuses.get(id) ?? "disconnected", getConnection: (id: string) => ({ channels: { agent: channels.get(id) }, profile: profiles.get(id) }) };
    const storage = unboundStorage(resources, path.join(root, "connections.enc"), async () => {});
    const router = new PackagedRouter(manager as unknown as SshSessionManager, { request: async () => { throw new Error("Remote operation reached local agent"); } }, storage);
    const negotiator = new PackagedNegotiator(router);
    const connect = async (profile: string) => {
      const client = new Client(); clients.push(client); client.on("error", error => transportErrors.push(error));
      client.connect({ host: "127.0.0.1", port: address.port, username: "crab-test", password, hostVerifier: (key: Buffer) => key.length === hostPublic.length && timingSafeEqual(key, hostPublic) });
      await once(client, "ready");
      const channel = await new Promise<ClientChannel>((resolve, reject) => client.exec("crab-desktop-agent", (error, stream) => error ? reject(error) : resolve(stream)));
      channel.on("data", (bytes: Buffer) => {
        try { new TextDecoder("utf-8", { fatal: true }).decode(bytes); }
        catch { splitUtf8Observed = true; }
      });
      profiles.set(profile, {}); channels.set(profile, channel); router.attachChannel(profile, channel);
      statuses.set(profile, "deploying");
      const capabilities = await negotiator.negotiate(profile);
      expect(capabilities).toMatchObject({ protocol: 1, version: expect.any(String) });
      expect(negotiator.isSupported(profile, "file_open", "read_range", "file_materialize", "rpc/cancel", "file_close")).toBe(true);
      expect(negotiator.isSupported(profile, "compress_file")).toBe(false);
      statuses.set(profile, "connected");
      const repo_id = `ssh://${profile}/source`;
      await router.route(repo_id, "open_local", { repo_id, path: source });
      return { repo_id, channel, child: children[children.length - 1] };
    };
    const first = await connect("one"), second = await connect("two");
    for (const index of [0, 1]) await expect.poll(() => existsSync(path.join(root, `index-${index}`))).toBe(true);
    expect(await router.route(first.repo_id, "read_text", { repo_id: first.repo_id, path: "unicode.txt" })).toMatchObject({ text, truncated: false });
    expect(await router.batchRpc(first.repo_id, [{ method: "ping", params: {} }, { method: "does_not_exist", params: {} }])).toMatchObject([{ result: { pong: expect.any(String) } }, { error: { code: -32601 } }]);
    // The live writer PID prevents stale-lock recovery from invalidating this
    // contention; Python flock matches the staging crate's POSIX fs4 lock.
    writer = spawn("python3", ["-c", "import fcntl,os,sys\nf=open(sys.argv[1],'a+')\nfcntl.flock(f,fcntl.LOCK_EX)\nf.seek(0);f.truncate();f.write(str(os.getpid()));f.flush()\nprint('held',flush=True)\nsys.stdin.readline()", path.join(source, ".crab/staging/lockfile")], { stdio: "pipe" });
    let writerError = ""; writer.stderr.on("data", data => { writerError += data.toString(); });
    await expect.poll(() => writer!.exitCode === null ? readFileSync(path.join(source, ".crab/staging/lockfile"), "utf8") : writerError).toBe(String(writer.pid));
    profiles.set("one", { operationTimeoutMs: 5000 });
    const timed = router.route(first.repo_id, "file_open", { repo_id: first.repo_id, path: "dataset.bin" });
    const timeout = expect(timed).rejects.toMatchObject({ code: "SSH_RPC_TIMEOUT" });
    let nativePid = 0;
    await expect.poll(() => { nativePid = descendants(first.child.pid!)[0] ?? 0; return nativePid; }).toBeGreaterThan(0);
    await timeout; expect(running(nativePid)).toBe(false); expect(running(writer.pid!)).toBe(true);
    profiles.set("one", {});
    const interrupted = router.route(first.repo_id, "file_open", { repo_id: first.repo_id, path: "dataset.bin" });
    const cancelled = expect(interrupted).rejects.toMatchObject({ code: 1010 });
    await expect.poll(() => { nativePid = descendants(first.child.pid!)[0] ?? 0; return nativePid; }).toBeGreaterThan(0);
    const exited = once(first.child, "exit"); first.channel.end(); await cancelled;
    expect(await exited).toEqual([0, null]); expect(running(nativePid)).toBe(false); expect(running(writer.pid!)).toBe(true);
    expect(await router.route(second.repo_id, "ping", {})).toMatchObject({ pong: expect.any(String) });
    await cleanExit(writer); writer = undefined;
    const progress: unknown[] = [];
    router.on("notification", event => { if (event.method === "progress/transfer") progress.push(event.params); });
    for (const file of ["dataset.bin", "lfs.bin"]) {
      const captured = await router.route(second.repo_id, "file_open", { repo_id: second.repo_id, path: file }) as { file_id: string };
      const range = await router.route(second.repo_id, "read_range", { repo_id: second.repo_id, file_id: captured.file_id, offset: content.length - 31, length: 31 }) as { bytes_b64: string };
      expect(Buffer.from(range.bytes_b64, "base64")).toEqual(content.subarray(-31)); expect(readFileSync(path.join(source, file))).toEqual(pointers.get(file));
      await router.route(second.repo_id, "file_materialize", { repo_id: second.repo_id, file_id: captured.file_id });
      expect(readFileSync(path.join(source, file)).equals(content)).toBe(true); expect(git(["show", `:${file}`])).toEqual(pointers.get(file));
      await router.route(second.repo_id, "file_close", { repo_id: second.repo_id, file_id: captured.file_id });
    }
    expect(progress).toEqual(expect.arrayContaining([expect.objectContaining({ written_bytes: content.length, total_bytes: content.length })]));
    await router.route(second.repo_id, "close", { repo_id: second.repo_id }); await cleanExit(second.child);
    expect(splitUtf8Observed).toBe(true);
    expect(transportErrors).toEqual([]);
    writeFileSync(test.info().outputPath("ssh-native-proof.json"), JSON.stringify({ bytesPerFile: content.length, formats: ["crab", "lfs"], timeout: "native-terminal-before-rejection", eof: "child-joined-before-agent-exit", utf8: "actual-byte-fragmented-SSH", capabilities: negotiator.getCached("two"), index: "pointer-preserved", platform: process.platform }, null, 2));
  } finally {
    if (writer) await cleanExit(writer);
    const exits = await Promise.allSettled(children.map(cleanExit)); clients.forEach(client => client.end());
    // ssh2 lacks net.Server.listening at runtime; always close the facade.
    if (server) await new Promise<void>((resolve, reject) => server!.close(error => error && (error as NodeJS.ErrnoException).code !== "ERR_SERVER_NOT_RUNNING" ? reject(error) : resolve()));
    rmSync(root, { recursive: true, force: true });
    for (const result of exits) if (result.status === "rejected") throw result.reason;
  }
});
