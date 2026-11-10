import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createHash, generateKeyPairSync } from "node:crypto";
import * as fs from "node:fs";
import { mkdtemp, mkdir, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { Readable } from "node:stream";
import { promisify } from "node:util";
import { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { CapabilityNegotiator } from "../electron/ssh-capabilities";
import { tmpdir } from "node:os";
import path from "node:path";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { Client, Server, utils, type Connection, type SFTPWrapper, type Stats } from "ssh2";
import { AgentDeployer } from "../electron/ssh-deployer";
import { KnownHostsManager, createHostKeyVerifier } from "../electron/ssh-known-hosts";
import { RemoteRuntimeStore } from "../electron/ssh-runtime";

const pem = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ type: "pkcs1", format: "pem" });
const key = utils.parseKey(pem);
if (key instanceof Error || Array.isArray(key)) throw new Error("Invalid owned SSH key");
const hostKey = key.getPublicSSH();
const digest = (data: Buffer) => createHash("sha256").update(data).digest("hex");

describe.skipIf(process.platform === "win32")("real SFTP runtime installation", () => {
  let directory: string; let remote: string; let local: string; let server: Server; let client: Client; let owner: AbortController;
  const connections = new Set<Connection>(); const children = new Set<ChildProcess>(); const descriptors = new Set<number>();
  let commands: string[]; let uploads: number; let acknowledged: number; let corrupt: boolean;
  let uname: string; let abortOnWrite: boolean; let stalledOperation: "OPEN" | "WRITE" | "CLOSE" | undefined; let written: (() => void) | undefined; let native: boolean; let exits: Array<number | null>;
  beforeEach(async () => {
    directory = await mkdtemp(path.join(tmpdir(), "crab-runtime-sftp-")); remote = path.join(directory, "remote ' 雪");
    local = path.join(directory, "app", "resources", "native", "mac-arm64"); await mkdir(remote); await mkdir(local, { recursive: true });
    const cli = Buffer.from("#!/bin/sh\nprintf 'owned-cli\\n'\n");
    const agent = Buffer.from('#!/bin/sh\n"$CRAB_BIN" --version\ngit-remote-crab --version\ncat\n');
    await writeFile(path.join(local, "crab"), cli); await writeFile(path.join(local, "crab-desktop-agent"), agent);
    await writeFile(path.join(local, "runtime.json"), JSON.stringify({ version: "2.0.0-alpha.1", platform: "darwin", arch: "arm64", hashes: { crab: digest(cli), "crab-desktop-agent": digest(agent) } }));
    commands = []; native = false; stalledOperation = undefined; written = undefined; exits = []; abortOnWrite = false; uploads = 0; acknowledged = 0; corrupt = false; uname = "Darwin\narm64\n"; owner = new AbortController();
    server = new Server({ hostKeys: [pem] }, connection => {
      connections.add(connection); connection.on("error", () => {}); connection.once("close", () => connections.delete(connection));
      connection.on("authentication", context => {
        if (context.method !== "publickey" || !context.key.data.equals(hostKey)) { context.reject(); return; }
        if (context.signature && (!context.blob || !key.verify(context.blob, context.signature, context.hashAlgo))) { context.reject(); return; }
        context.accept();
      });
      connection.on("ready", () => connection.on("session", accept => {
        const session = accept();
        session.on("sftp", acceptSftp => bindSftp(acceptSftp()));
        session.on("exec", (acceptCommand, _reject, info) => {
          const channel = acceptCommand(); channel.on("error", () => {}); commands.push(info.command);
          if (info.command === "uname -s && uname -m") { channel.write(uname); channel.exit(0); channel.end(); return; }
          if (info.command.includes("shasum") && acknowledged !== uploads) throw new Error("Hash attempted before SFTP CLOSE acknowledgement");
          const child = spawn("/bin/sh", ["-c", info.command], { cwd: remote, detached: true, env: { HOME: remote, PATH: process.env.PATH, TMPDIR: directory, LANG: "en_US.UTF-8" } });
          children.add(child); channel.pipe(child.stdin); child.stdout.pipe(channel, { end: false }); child.stderr.pipe(channel.stderr, { end: false });
          channel.once("close", () => {
            if (!children.has(child)) return;
            child.stdin.end(); child.stdout.resume(); child.stderr.resume();
            const deadline = setTimeout(() => { try { process.kill(-child.pid!, "SIGTERM"); } catch { /* The owned group already exited. */ } }, 3000);
            child.once("close", () => clearTimeout(deadline));
          });
          child.once("error", () => { channel.exit(1); channel.end(); });
          child.once("close", code => { if (native) exits.push(code); children.delete(child); channel.exit(code ?? 1); channel.end(); });
        });
      }));
    });
    await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve)); const address = server.address();
    if (!address || typeof address === "string") throw new Error("Owned fixture did not bind");
    const hosts = new KnownHostsManager(path.join(directory, "pins.json"), path.join(directory, "user"), path.join(directory, "global"));
    hosts.accept("127.0.0.1", address.port, hostKey); const verify = createHostKeyVerifier(hosts, async () => false);
    client = new Client(); client.on("error", () => {});
    await new Promise<void>((resolve, reject) => {
      client.once("ready", resolve); client.once("error", reject);
      client.connect({ host: "127.0.0.1", port: address.port, username: "fixture", privateKey: pem,
        hostVerifier: (candidate: Buffer, done: (accepted: boolean) => void) => { void verify("127.0.0.1", address.port, candidate, owner.signal).then(() => done(true), () => done(false)); },
      });
    });
  });
  afterEach(async () => {
    owner.abort(); client?.destroy(); for (const connection of connections) connection.end(); connections.clear();
    const closed = [...children].map(child => new Promise<void>(resolve => { child.once("close", () => resolve()); try { process.kill(-child.pid!, "SIGTERM"); } catch { /* The owned group already exited. */ } })); await Promise.all(closed); children.clear();
    for (const descriptor of descriptors) { try { fs.closeSync(descriptor); } catch { /* Closed by the SFTP peer. */ } } descriptors.clear();
    if (server?.listening) await new Promise<void>(resolve => server.close(() => resolve()));
    await rm(directory, { recursive: true, force: true });
  });
  function bindSftp(sftp: SFTPWrapper) {
    const handles = new Map<string, number>();
    const safe = (file: string) => { const resolved = path.resolve(file); if (resolved !== remote && !resolved.startsWith(`${remote}${path.sep}`)) throw new Error("Fixture SFTP escaped its owned root"); return resolved; };
    const status = (id: number, error?: NodeJS.ErrnoException | null) => sftp.status(id, !error ? 0 : error.code === "ENOENT" ? 2 : error.code === "EACCES" ? 3 : 4);
    const attrs = (id: number, error: NodeJS.ErrnoException | null, stat: fs.Stats) => error ? status(id, error) : sftp.attrs(id, { mode: stat.mode, size: stat.size, uid: stat.uid, gid: stat.gid, atime: Math.floor(stat.atimeMs / 1000), mtime: Math.floor(stat.mtimeMs / 1000) });
    sftp.on("REALPATH", (id: number, _file: string) => sftp.name(id, [{ filename: remote, longname: remote, attrs: {} as Stats }]));
    sftp.on("LSTAT", (id: number, file: string) => fs.lstat(safe(file), (error, value) => attrs(id, error, value)));
    sftp.on("STAT", (id: number, file: string) => fs.stat(safe(file), (error, value) => attrs(id, error, value)));
    sftp.on("MKDIR", (id: number, file: string, value: { mode?: number }) => fs.mkdir(safe(file), { mode: value.mode }, error => status(id, error)));
    sftp.on("OPEN", (id: number, file: string, flags: number, value: { mode?: number }) => {
      const mode = utils.sftp.flagsToString(flags); if (!mode) { sftp.status(id, 4); return; }
      fs.open(safe(file), mode, value.mode, (error, descriptor) => {
        if (error) { status(id, error); return; }
        uploads += 1; const handle = Buffer.from(String(descriptor)); handles.set(handle.toString(), descriptor); descriptors.add(descriptor); if (stalledOperation === "OPEN") { written?.(); return; } sftp.handle(id, handle);
      });
    });
    sftp.on("WRITE", (id: number, handle: Buffer, offset: number, data: Buffer) => {
      const bytes = corrupt ? Buffer.alloc(data.length, 0) : data;
      const write = (position: number) => fs.write(handles.get(handle.toString())!, bytes, position, bytes.length - position, offset + position, (error, count) => {
        if (!error && count > 0 && position + count < bytes.length) { write(position + count); return; }
        if (stalledOperation === "WRITE") { written?.(); return; }
        status(id, error || (count === 0 ? Object.assign(new Error("Fixture short write"), { code: "EIO" }) : undefined));
        if (abortOnWrite) owner.abort(new Error("Connection retired during upload"));
      });
      write(0);
    });
    sftp.on("FSETSTAT", (id: number, handle: Buffer, value: { mode?: number }) => fs.fchmod(handles.get(handle.toString())!, value.mode ?? 0o700, error => status(id, error)));
    sftp.on("FSTAT", (id: number, handle: Buffer) => fs.fstat(handles.get(handle.toString())!, (error, value) => attrs(id, error, value)));
    sftp.on("CLOSE", (id: number, handle: Buffer) => {
      const descriptor = handles.get(handle.toString())!; handles.delete(handle.toString()); descriptors.delete(descriptor);
      fs.close(descriptor, error => { if (stalledOperation === "CLOSE") { written?.(); return; } acknowledged += 1; status(id, error); });
    });
    sftp.once("close", () => {
      // SSH2 exposes the SFTP subsystem directly; its server owner must return
      // CHANNEL_CLOSE after receiving it, as an SSH daemon would.
      sftp.end();
      for (const descriptor of handles.values()) { try { fs.closeSync(descriptor); } catch { /* A CLOSE request may already have retired it. */ } descriptors.delete(descriptor); }
      handles.clear();
    });
    sftp.on("RENAME", (id: number, from: string, to: string) => fs.rename(safe(from), safe(to), error => status(id, error)));
    sftp.on("REMOVE", (id: number, file: string) => fs.unlink(safe(file), error => status(id, error)));
    sftp.on("RMDIR", (id: number, file: string) => fs.rmdir(safe(file), error => status(id, error)));
  }
  function deployer() {
    const store = new RemoteRuntimeStore({ packaged: false, appPath: path.join(directory, "app"), version: "2.0.0-alpha.1", cacheDirectory: path.join(directory, "cache") });
    return new AgentDeployer(store.resolve.bind(store));
  }
  async function start() {
    const channel = await deployer().ensureAgent(client, owner.signal);
    const output = await new Promise<string>((resolve, reject) => { let data = ""; channel.on("data", (chunk: Buffer) => { data += chunk.toString(); if (data.split("owned-cli").length === 3) resolve(data); }); channel.once("error", reject); });
    return { channel, output };
  }
  it("uploads and starts the matching CLI/sidecar and Git helper in an immutable directory", async () => {
    const { channel, output } = await start(); channel.end();
    const parent = path.join(remote, ".crab/desktop/runtimes/2.0.0-alpha.1/darwin-arm64");
    const versions = await readdir(parent);
    expect({ output, uploads, acknowledged, versions, cli: await readFile(path.join(parent, versions[0], "crab")), helper: await readFile(path.join(parent, versions[0], "git-remote-crab")), commands: commands.some(command => /pgrep|\.crab\/bin/.test(command)) }).toEqual({ output: "owned-cli\nowned-cli\n", uploads: 2, acknowledged: 2, versions: [expect.stringMatching(/^[a-f0-9]{64}$/)], cli: await readFile(path.join(local, "crab")), helper: await readFile(path.join(local, "crab")), commands: false });
  });
  it("reuses a verified runtime while another session is running", async () => {
    const first = await start(); const second = await start();
    expect({ uploads, outputs: [first.output, second.output] }).toEqual({ uploads: 2, outputs: ["owned-cli\nowned-cli\n", "owned-cli\nowned-cli\n"] }); first.channel.end(); second.channel.end();
  });
  it("does not start a corrupted upload and removes its own staging files", async () => {
    corrupt = true; await expect(deployer().ensureAgent(client, owner.signal)).rejects.toThrow("checksum mismatch");
    expect({ starts: commands.filter(command => command.startsWith("exec env")).length, files: await readdir(path.join(remote, ".crab/desktop/runtimes/2.0.0-alpha.1/darwin-arm64")) }).toEqual({ starts: 0, files: [] });
  });
  it("owns a fatal SFTP subsystem error until physical closure", async () => {
    const open = client.sftp.bind(client);
    vi.spyOn(client, "sftp").mockImplementation(callback => open((error, sftp) => {
      callback(error, sftp);
      if (!error) setImmediate(() => sftp.emit("error", new Error("Owned SFTP protocol failure")));
    }));
    await expect(deployer().ensureAgent(client, owner.signal)).rejects.toMatchObject({ code: "UPLOAD_FAILED", cause: { message: "Owned SFTP protocol failure" } });
  });
  it("rejects unsupported architectures before resolving or uploading a runtime", async () => {
    uname = "Linux\ns390x\n"; await expect(deployer().ensureAgent(client, owner.signal)).rejects.toMatchObject({ code: "WRONG_ARCH" }); expect(uploads).toBe(0);
  });
  it("retires an interrupted upload through its remote staging guard", async () => {
    abortOnWrite = true; await expect(deployer().ensureAgent(client, owner.signal)).rejects.toThrow("Connection retired during upload");
    const parent = path.join(remote, ".crab/desktop/runtimes/2.0.0-alpha.1/darwin-arm64");
    await vi.waitFor(async () => expect(await readdir(parent)).toEqual([]));
  });

  it.each(["OPEN", "WRITE", "CLOSE"] as const)("bounds an upload whose SSH connection stays alive but SFTP stops acknowledging %s", async operation => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    let result: unknown; const stalled = new Promise<void>(resolve => { written = resolve; }); stalledOperation = operation;
    const installing = deployer().ensureAgent(client, owner.signal).then(value => { result = value; }, error => { result = error; });
    try {
      await stalled; await vi.advanceTimersByTimeAsync(30_001);
      await vi.waitFor(() => expect(result).toMatchObject({ code: "DEPLOY_TIMED_OUT" }));
      expect(await readdir(path.join(remote, ".crab/desktop/runtimes/2.0.0-alpha.1/darwin-arm64"))).toEqual([]);
    } finally { owner.abort(new Error("Owned stall fixture retired")); vi.useRealTimers(); await installing; }
  });

  it.skipIf(!process.env.CRAB_AGENT_LIVE_ROOT)("downloads, uploads and starts the built runtime through the real session owner", async () => {
    native = true; const run = promisify(execFile);
    const { version } = JSON.parse(await readFile(path.resolve("package.json"), "utf8")) as { version: string };
    uname = `${(await run("uname", ["-s"])).stdout.trim()}\n${(await run("uname", ["-m"])).stdout.trim()}\n`;
    const assets = path.resolve("release/remote-runtime"); const requests: string[] = [];
    const store = new RemoteRuntimeStore({ packaged: true, appPath: directory, version, cacheDirectory: path.join(directory, "download-cache") }, async input => {
      const name = new URL(String(input)).pathname.split("/").at(-1)!; requests.push(name);
      const body = Readable.toWeb(fs.createReadStream(path.join(assets, name)));
      return new Response(body as unknown as ReadableStream<Uint8Array>);
    });
    const address = server.address(); if (!address || typeof address === "string") throw new Error("Owned fixture unavailable");
    const identity = path.join(directory, "identity.pem"); await writeFile(identity, pem, { mode: 0o600 });
    const hosts = new KnownHostsManager(path.join(directory, "manager-pins"), path.join(directory, "manager-user"), path.join(directory, "manager-global"));
    hosts.accept("127.0.0.1", address.port, hostKey);
    const manager = new SshSessionManager(createHostKeyVerifier(hosts, async () => false));
    const router = new RpcRouter(manager, { request: async () => { throw new Error("Local fallback must not run"); } });
    const negotiator = new CapabilityNegotiator(router);
    manager.setAgentDependencies({ createDeployer: () => new AgentDeployer(store.resolve.bind(store)), rpcRouter: router, capabilityNegotiator: negotiator });
    let completedWithCapabilities = false;
    manager.on("ssh:deploy-progress", value => { if (value.stage === "starting" && value.percent === 100) completedWithCapabilities = Boolean(negotiator.getCached("runtime")); });
    try {
      await manager.connect({ id: "runtime", label: "Owned native runtime", host: "127.0.0.1", port: address.port, username: "fixture", authMethod: { type: "key", keyPath: identity }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: false, autoDeployAgent: true, tags: [], portForwards: [], createdAt: Date.now() });
      expect({ status: manager.getStatus("runtime"), version: negotiator.getCached("runtime")?.version, downloads: requests.length, uploads, completedWithCapabilities }).toEqual({ status: "connected", version, downloads: 3, uploads: 2, completedWithCapabilities: true });
      const repo = path.join(remote, "repo"); const worktree = path.join(remote, "task-worktree"); await mkdir(repo);
      const env = { HOME: remote, PATH: process.env.PATH, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };
      await run("git", ["init", "--template="], { cwd: repo, env }); await writeFile(path.join(repo, "tracked.txt"), "before\n");
      await run("git", ["add", "tracked.txt"], { cwd: repo, env });
      await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--no-gpg-sign", "-m", "baseline"], { cwd: repo, env });
      await router.sendToRemoteAgent("runtime", "open_local", { repo_id: "owned", path: repo });
      const created = await router.sendToRemoteAgent("runtime", "git_worktree_add", { repo_id: "owned", path: worktree, branch: "owned-task", create_branch: true }) as { created: boolean; path: string };
      await writeFile(path.join(worktree, "tracked.txt"), "after-native-deployment\n");
      const comparison = await router.sendToRemoteAgent("runtime", "task_compute_diff", { worktree_path: worktree, base_ref: "HEAD" }) as { diff: string };
      expect({ created: created.created, path: created.path, content: await readFile(path.join(worktree, "tracked.txt"), "utf8"), diff: comparison.diff }).toEqual({ created: true, path: await realpath(worktree), content: "after-native-deployment\n", diff: expect.stringContaining("+after-native-deployment") });
      await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "ssh-native-runtime.json"), JSON.stringify({ downloads: requests, uploads, created, diff: comparison.diff, status: manager.getStatus("runtime") }));
    } finally { await manager.disconnect("runtime"); }
    await vi.waitFor(() => expect(children.size).toBe(0), { timeout: 10_000 });
    expect(exits.every(code => code === 0)).toBe(true);
  }, 120_000);

});
