import { mockStorageConnections } from "./mock-storage-connections";
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
import { KnownHostsManager, createHostKeyVerifier, type HostKeyVerifier } from "../electron/ssh-known-hosts";
import { RemoteRuntimeStore } from "../electron/ssh-runtime";
import { RemoteFileBrowser } from "../electron/ssh-file-browser";
import { ProviderRegistry } from "../electron/provider-registry";
import { RemoteProvider } from "../electron/remote-provider";
import { RemoteFileEditor } from "../electron/ssh-file-editor";
import { RemoteGitManager } from "../electron/ssh-remote-git";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";

const pem = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ type: "pkcs1", format: "pem" });
const key = utils.parseKey(pem);
if (key instanceof Error || Array.isArray(key)) throw new Error("Invalid owned SSH key");
const hostKey = key.getPublicSSH();
const digest = (data: Buffer) => createHash("sha256").update(data).digest("hex");

describe.skipIf(process.platform === "win32")("real SFTP runtime installation", () => {
  let directory: string; let remote: string; let local: string; let server: Server; let client: Client; let owner: AbortController;
  const connections = new Set<Connection>(); const children = new Set<ChildProcess>(); const descriptors = new Set<number>();
  let commands: string[]; let uploads: number; let acknowledged: number; let corrupt: boolean;
  let verifier: HostKeyVerifier;
  let traceEnabled = false; let traceStarted = 0;
  const trace = (event: string, value: unknown = undefined) => { if (traceEnabled) console.log("[DEBUG-intel-sftp]", JSON.stringify({ ms: Math.round(performance.now() - traceStarted), event, value })); };
  let uname: string; let abortOnWrite: boolean; let stalledOperation: "OPEN" | "WRITE" | "CLOSE" | undefined; let written: (() => void) | undefined; let native: boolean; let exits: Array<number | null>; let diagnostics: string[];
  beforeEach(async () => {
    directory = await mkdtemp(path.join(process.env.CRAB_AGENT_LIVE_ROOT ?? tmpdir(), "crab-runtime-sftp-")); remote = path.join(directory, "remote ' 雪");
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(directory, "settings.json"));
    local = path.join(directory, "app", "resources", "native", "mac-arm64"); await mkdir(remote); await mkdir(local, { recursive: true });
    const cli = Buffer.from("#!/bin/sh\nprintf 'owned-cli\\n'\n");
    const agent = Buffer.from('#!/bin/sh\n"$CRAB_BIN" --version\ngit-remote-crab --version\ncat\n');
    await writeFile(path.join(local, "crab"), cli); await writeFile(path.join(local, "crab-desktop-agent"), agent);
    await writeFile(path.join(local, "runtime.json"), JSON.stringify({ version: "2.0.0-alpha.1", platform: "darwin", arch: "arm64", hashes: { crab: digest(cli), "crab-desktop-agent": digest(agent) } }));
    commands = []; diagnostics = []; native = false; stalledOperation = undefined; written = undefined; exits = []; abortOnWrite = false; uploads = 0; acknowledged = 0; corrupt = false; uname = "Darwin\narm64\n"; owner = new AbortController();
    server = new Server({ hostKeys: [pem] }, connection => {
      // OpenSSH enables TCP_NODELAY for non-PTY sessions too. Model that peer
      // policy so Linux delayed ACKs do not dominate this transport fixture.
      // ssh2 exposes this server method but its Connection type omits it.
      (connection as Connection & { setNoDelay(noDelay: boolean): void }).setNoDelay(true);
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
          const channel = acceptCommand(); channel.on("error", () => {}); commands.push(info.command); trace("exec", info.command.slice(0, 100));
          if (info.command === "uname -s && uname -m") { channel.write(uname); channel.exit(0); channel.end(); return; }
          if (info.command.includes("shasum") && acknowledged !== uploads) throw new Error("Hash attempted before SFTP CLOSE acknowledgement");
          trace("shell-spawn"); const child = spawn("/bin/sh", ["-c", info.command], { cwd: remote, detached: true, env: { HOME: remote, PATH: process.env.PATH, TMPDIR: directory, LANG: "en_US.UTF-8", ...(native ? { AWS_SESSION_TOKEN: "fixture-ambient-token" } : {}) } });
          children.add(child); child.stderr.on("data", (chunk: Buffer) => { if (native) diagnostics.push(chunk.toString()); }); channel.pipe(child.stdin); child.stdout.pipe(channel, { end: false }); child.stderr.pipe(channel.stderr, { end: false });
          channel.once("close", () => {
            if (!children.has(child)) return;
            child.stdin.end(); child.stdout.resume(); child.stderr.resume();
            const deadline = setTimeout(() => { try { process.kill(-child.pid!, "SIGTERM"); } catch { /* The owned group already exited. */ } }, 3000);
            child.once("close", () => clearTimeout(deadline));
          });
          child.once("error", () => { channel.exit(1); channel.end(); });
          child.once("close", code => { trace("shell-close", { code }); if (native) exits.push(code); children.delete(child); channel.exit(code ?? 1); channel.end(); });
        });
      }));
    });
    await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve)); const address = server.address();
    if (!address || typeof address === "string") throw new Error("Owned fixture did not bind");
    const hosts = new KnownHostsManager(path.join(directory, "pins.json"), path.join(directory, "user"), path.join(directory, "global"));
    hosts.accept("127.0.0.1", address.port, hostKey); verifier = createHostKeyVerifier(hosts, async () => false);
    client = new Client(); client.on("error", () => {});
    await new Promise<void>((resolve, reject) => {
      client.once("ready", () => { client.setNoDelay(true); resolve(); }); client.once("error", reject);
      client.connect({ host: "127.0.0.1", port: address.port, username: "fixture", privateKey: pem,
        hostVerifier: (candidate: Buffer, done: (accepted: boolean) => void) => { void verifier("127.0.0.1", address.port, candidate, owner.signal).then(() => done(true), () => done(false)); },
      });
    });
  });
  afterEach(async () => {
    owner.abort(); client?.destroy(); for (const connection of connections) connection.end(); connections.clear();
    const closed = [...children].map(child => new Promise<void>(resolve => { child.once("close", () => resolve()); try { process.kill(-child.pid!, "SIGTERM"); } catch { /* The owned group already exited. */ } })); await Promise.all(closed); children.clear();
    for (const descriptor of descriptors) { try { fs.closeSync(descriptor); } catch { /* Closed by the SFTP peer. */ } } descriptors.clear();
    if (server?.listening) await new Promise<void>(resolve => server.close(() => resolve()));
    vi.unstubAllEnvs();
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
    const instance = new AgentDeployer(store.resolve.bind(store)); instance.on("ssh:deploy-progress", event => trace("deploy", event)); return instance;
  }
  async function start(target = client) {
    trace("ensure-start"); const channel = await deployer().ensureAgent(target, owner.signal); trace("ensure-return");
    const output = await new Promise<string>((resolve, reject) => { let data = ""; channel.on("data", (chunk: Buffer) => { data += chunk.toString(); trace("output", chunk.length); if (data.split("owned-cli").length === 3) resolve(data); }); channel.once("error", reject); });
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
  it("streams a multi-megabyte runtime through the normal deployment boundary", async () => {
    traceEnabled = true; traceStarted = performance.now(); trace("large-start");
    const padding = Buffer.concat([Buffer.from("#"), Buffer.alloc(2 * 1024 * 1024, 120), Buffer.from("\n")]);
    const cli = Buffer.concat([await readFile(path.join(local, "crab")), padding]);
    const agent = Buffer.concat([await readFile(path.join(local, "crab-desktop-agent")), padding]);
    await writeFile(path.join(local, "crab"), cli); await writeFile(path.join(local, "crab-desktop-agent"), agent);
    await writeFile(path.join(local, "runtime.json"), JSON.stringify({ version: "2.0.0-alpha.1", platform: "darwin", arch: "arm64", hashes: { crab: digest(cli), "crab-desktop-agent": digest(agent) } }));
    const address = server.address(); if (!address || typeof address === "string") throw new Error("Owned fixture unavailable");
    const identity = path.join(directory, "large-identity.pem"); await writeFile(identity, pem, { mode: 0o600 });
    const manager = new SshSessionManager(verifier);
    try {
      await manager.connect({ id: "large", label: "Owned large runtime", host: "127.0.0.1", port: address.port, username: "fixture", authMethod: { type: "key", keyPath: identity }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: false, autoDeployAgent: false, tags: [], portForwards: [], createdAt: Date.now() });
      trace("connected"); const connection = manager.getConnection("large"); if (!connection) throw new Error("Managed SSH connection missing");
      const { channel, output } = await start(connection.client); channel.end();
      expect({ output, uploads, acknowledged }).toEqual({ output: "owned-cli\nowned-cli\n", uploads: 2, acknowledged: 2 });
    } finally { trace("disconnect-start"); await manager.disconnect("large"); trace("disconnect-done"); traceEnabled = false; }
  }, 30_000);
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
    native = true; const run = promisify(execFile); let cachePaths: string[] = [];
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
    const router = new RpcRouter(manager, { request: async () => { throw new Error("Local fallback must not run"); } }, mockStorageConnections);
    const negotiator = new CapabilityNegotiator(router);
    manager.setAgentDependencies({ createDeployer: () => new AgentDeployer(store.resolve.bind(store)), rpcRouter: router, capabilityNegotiator: negotiator });
    // Persist before waits so a runner deadline still leaves the last phase
    // and owned process diagnostics, even when the test's finally cannot finish.
    const progress = (phase: string) => fs.writeFileSync(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "ssh-native-progress.json"), JSON.stringify({
      phase, uploads, acknowledged, commands: commands.slice(-20), exits, children: [...children].map(child => child.pid), diagnostics: diagnostics.join("").slice(-64 * 1024),
    }));
    let completedWithCapabilities = false;
    manager.on("ssh:deploy-progress", value => { progress(`${value.stage}:${value.percent}`); if (value.stage === "starting" && value.percent === 100) completedWithCapabilities = Boolean(negotiator.getCached("runtime")); });
    try {
      progress("connect");
      await manager.connect({ id: "runtime", label: "Owned native runtime", host: "127.0.0.1", port: address.port, username: "fixture", authMethod: { type: "key", keyPath: identity }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: true, autoDeployAgent: true, tags: [], portForwards: [], createdAt: Date.now() });
      expect({ status: manager.getStatus("runtime"), version: negotiator.getCached("runtime")?.version, downloads: requests.length, uploads, completedWithCapabilities }).toEqual({ status: "connected", version, downloads: 3, uploads: 2, completedWithCapabilities: true });
      const repo = path.join(remote, "repo"); const worktree = path.join(remote, "task-worktree"); await mkdir(repo);
      const env = { HOME: remote, PATH: process.env.PATH, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };
      await run("git", ["init", "--template=", "--initial-branch=main"], { cwd: repo, env }); await writeFile(path.join(repo, "tracked.txt"), "before\n");
      await run("git", ["add", "tracked.txt"], { cwd: repo, env });
      await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--no-gpg-sign", "-m", "baseline"], { cwd: repo, env });
      const provider = new RemoteProvider({ router, sessionManager: manager, capabilityNegotiator: negotiator, fileEditor: new RemoteFileEditor(router, manager), gitManager: new RemoteGitManager(router, manager) });
      const browser = new RemoteFileBrowser(manager, new ProviderRegistry(), provider);
      progress("open-repository");
      const opened = await browser.openAsRepo("runtime", repo) as { repo_id: string; is_remote: boolean };
      const repoId = `ssh://runtime${repo}`;
      expect({ id: opened.repo_id, remote: opened.is_remote }).toEqual({ id: repoId, remote: true });
      const processFactory = remoteAgentProcessFactory(router, "runtime");
      progress("native-stdio");
      const nativeProcess = processFactory("/bin/sh", ["-c", "cat > native-stdin-proof.bin"], { cwd: repo });
      const processErrors: string[] = [];
      nativeProcess.process.on("error", (error: Error) => processErrors.push(error.message));
      nativeProcess.process.stdout?.resume(); nativeProcess.process.stderr?.resume();
      const nativeBytes = Buffer.from("雪🙂\u0000".repeat(10_000));
      await new Promise<void>((resolve, reject) => nativeProcess.process.stdin!.end(nativeBytes, (error?: Error | null) => error ? reject(error) : resolve()));
      await nativeProcess.closed;
      expect({ bytes: await readFile(path.join(repo, "native-stdin-proof.bin")), errors: processErrors }).toEqual({ bytes: nativeBytes, errors: [] });
      const created = await router.sendToRemoteAgent("runtime", "git_worktree_add", { repo_id: repoId, path: worktree, branch: "owned-task", create_branch: true }) as { created: boolean; path: string };
      await writeFile(path.join(worktree, "tracked.txt"), "after-native-deployment\n");
      const comparison = await router.sendToRemoteAgent("runtime", "task_compute_diff", { worktree_path: worktree, base_ref: "HEAD" }) as { diff: string };
      expect({ created: created.created, path: created.path, content: await readFile(path.join(worktree, "tracked.txt"), "utf8"), diff: comparison.diff }).toEqual({ created: true, path: await realpath(worktree), content: "after-native-deployment\n", diff: expect.stringContaining("+after-native-deployment") });
      const taskIds = ["12345678-1111-4111-8111-111111111111", "12345678-2222-4222-8222-222222222222"];
      progress("task-workspaces");
      const shortPath = path.join(remote, ".crab", "worktrees", "12345678");
      await mkdir(shortPath, { recursive: true });
      await writeFile(path.join(shortPath, "keep.txt"), "unrelated user bytes");
      const taskWorktrees: Array<{ path: string; branch: string; base_ref: string }> = [];
      for (const taskId of taskIds) {
        const result = await router.route(repoId, "worktree_create", { repo_id: repoId, task_id: taskId, branch: `agent/${taskId}`, base_ref: "HEAD" }) as { path: string; branch: string; base_ref: string };
        expect({ path: result.path, unrelated: await readFile(path.join(shortPath, "keep.txt"), "utf8") }).toEqual({ path: path.join(remote, ".crab", "worktrees", taskId), unrelated: "unrelated user bytes" });
        taskWorktrees.push(result);
      }
      const firstTask = taskWorktrees[0];
      const bystander = path.join(repo, "unregistered-directory");
      await mkdir(bystander);
      await writeFile(path.join(bystander, "keep.txt"), "unrelated directory bytes");
      for (const method of ["task_discard", "task_apply", "worktree_destroy"]) {
        await expect(router.route(repoId, method, { repo_id: repoId, task_id: taskIds[0], branch: firstTask.branch, worktree_path: bystander, path: bystander, base_ref: firstTask.base_ref })).rejects.toThrow();
      }
      expect(await readFile(path.join(bystander, "keep.txt"), "utf8")).toBe("unrelated directory bytes");
      for (const method of ["task_discard", "task_apply"]) {
        for (const wrong of [{ task_id: taskIds[1] }, { branch: "main" }, { worktree_path: repo }]) {
          await expect(router.route(repoId, method, { repo_id: repoId, task_id: taskIds[0], branch: firstTask.branch, worktree_path: firstTask.path, base_ref: firstTask.base_ref, ...wrong })).rejects.toThrow();
        }
      }
      await writeFile(path.join(firstTask.path, "unsaved.txt"), "task user bytes");
      await expect(router.route(repoId, "worktree_create", { repo_id: repoId, task_id: taskIds[0], branch: firstTask.branch, base_ref: "HEAD" })).rejects.toThrow();
      expect(await readFile(path.join(firstTask.path, "unsaved.txt"), "utf8")).toBe("task user bytes");
      const invalidId = "12345678-3333-4333-8333-333333333333";
      await expect(router.route(repoId, "worktree_create", { repo_id: repoId, task_id: invalidId, branch: `agent/${invalidId}`, base_ref: "refs/heads/missing-task-baseline" })).rejects.toThrow();
      expect(fs.existsSync(path.join(remote, ".crab", "worktrees", invalidId))).toBe(false);
      for (const taskId of ["../escape", "1234567雪"]) await expect(router.route(repoId, "worktree_create", { repo_id: repoId, task_id: taskId, branch: "agent/invalid", base_ref: "HEAD" })).rejects.toThrow("UUID");
      const concurrentId = "12345678-4444-4444-8444-444444444444";
      const attempts = await Promise.allSettled([0, 1].map(() => router.route(repoId, "worktree_create", { repo_id: repoId, task_id: concurrentId, branch: `agent/${concurrentId}`, base_ref: "HEAD" })));
      expect(attempts.map(result => result.status).sort()).toEqual(["fulfilled", "rejected"]);
      const refusedId = "12345678-5555-4555-8555-555555555555";
      await expect(router.route(repoId, "worktree_create", { repo_id: repoId, task_id: refusedId, branch: "owned-task", base_ref: "HEAD" })).rejects.toThrow();
      expect({ reservation: fs.existsSync(path.join(remote, ".crab", "worktrees", refusedId)), original: await readFile(path.join(worktree, "tracked.txt"), "utf8") }).toEqual({ reservation: false, original: "after-native-deployment\n" });
      await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "task-workspace-creation.json"), JSON.stringify({ taskWorktrees, preserved: await readFile(path.join(shortPath, "keep.txt"), "utf8") }));
      // ARM64 runtime production uses plain Node without Electron's SQLite ABI.
      // Keep the Electron task/database probe on the matching Electron runner.
      if (process.versions.electron) {
        const { Orchestrator } = await import("../electron/orchestrator");
        const orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), rpcRouter: router });
        try {
          const task = await orchestrator.createTask("Remote baseline proof", "codex", repoId, "HEAD");
          await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "--no-gpg-sign", "-m", "Move HEAD"], { cwd: repo, env });
          const moved = (await run("git", ["rev-parse", "HEAD"], { cwd: repo, env })).stdout.trim();
          const saved = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") });
          try {
            expect({ changed: task.baseRef !== moved, base: saved.getTask(task.id)?.baseRef, branch: task.branch, file: await readFile(path.join(task.worktreePath, "tracked.txt"), "utf8") }).toEqual({ changed: true, base: firstTask.base_ref, branch: `agent/${task.id}`, file: "before\n" });
          } finally { await saved.shutdown(); }
          await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "task-workspace-creation.json"), JSON.stringify({ taskWorktrees, task, moved, preserved: await readFile(path.join(shortPath, "keep.txt"), "utf8") }));
        } finally { await orchestrator.shutdown(); }
      }
      const profile = manager.getConnection("runtime")!.profile;
      await manager.connect({ ...profile, id: "concurrent", autoReconnect: false });
      try {
        const first = await router.sendToRemoteAgent("runtime", "indexer_status", {}) as { db_path: string; running: boolean };
        const second = await router.sendToRemoteAgent("concurrent", "indexer_status", {}) as { db_path: string; running: boolean };
        cachePaths = [first.db_path, second.db_path];
        expect({ distinct: first.db_path !== second.db_path, running: [first.running, second.running] }).toEqual({ distinct: true, running: [true, true] });
        const otherRepo = await browser.openAsRepo("concurrent", repo) as { repo_id: string };
        const busyPath = path.join(remote, ".crab", "worktrees", concurrentId);
        const ownedFactory = remoteAgentProcessFactory(router, "runtime", concurrentId);
        const writer = ownedFactory("/bin/sh", ["-c", "exec sleep 120"], { cwd: busyPath });
        writer.process.on("error", () => {}); writer.process.stdout?.resume(); writer.process.stderr?.resume();
        await new Promise<void>((resolve, reject) => writer.process.stdin!.write(Buffer.alloc(0), error => error ? reject(error) : resolve()));
        const discard = { repo_id: otherRepo.repo_id, task_id: concurrentId, branch: `agent/${concurrentId}`, worktree_path: busyPath };
        try { await expect(router.route(otherRepo.repo_id, "task_discard", discard)).rejects.toThrow("active writer"); }
        finally { writer.process.kill(); await writer.closed; }
        const removed = await router.route(otherRepo.repo_id, "task_discard", discard);
        expect({ removed, exists: fs.existsSync(busyPath) }).toEqual({ removed: { discarded: true, worktree_removed: true, branch_deleted: true, cleanup_error: null }, exists: false });
        // A recreated directory is not the old task. Required identity cannot
        // fall through to an unowned native spawn after its .git pointer goes.
        await mkdir(busyPath);
        await writeFile(path.join(busyPath, "keep.txt"), "recreated directory bytes");
        const stale = ownedFactory("/bin/sh", ["-c", "printf unexpected > unexpected.txt"], { cwd: busyPath });
        stale.process.on("error", () => {});
        await expect(stale.closed).rejects.toThrow();
        expect(await readdir(busyPath)).toEqual(["keep.txt"]);

        const applyId = "12345678-6666-4666-8666-666666666666";
        const apply = await router.route(repoId, "worktree_create", { repo_id: repoId, task_id: applyId, branch: `agent/${applyId}`, base_ref: "HEAD" }) as { path: string; branch: string; base_ref: string };
        await writeFile(path.join(apply.path, "applied.txt"), "confirmed merge\n");
        await run("git", ["add", "applied.txt"], { cwd: apply.path, env });
        await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--no-gpg-sign", "-m", "Owned task change"], { cwd: apply.path, env });
        await run("git", ["config", "user.name", "Fixture"], { cwd: repo, env });
        await run("git", ["config", "user.email", "fixture@example.invalid"], { cwd: repo, env });
        await run("git", ["worktree", "lock", "--reason", "owned cleanup rejection", apply.path], { cwd: repo, env });
        const hookReceipt = path.join(repo, "selected-store-hook.txt");
        const hooks = path.join(repo, ".git", "hooks"); await mkdir(hooks, { recursive: true });
        const hook = `#!/bin/sh
[ "$AWS_ACCESS_KEY_ID" = fixture-selected-access ] || exit 87
[ "$AWS_SECRET_ACCESS_KEY" = fixture-selected-secret ] || exit 88
[ -z "$AWS_SESSION_TOKEN" ] || exit 89
printf verified > '${hookReceipt.replaceAll("'", "'\\''")}'
`;
        await writeFile(path.join(hooks, "pre-merge-commit"), hook, { mode: 0o755 });
        const selectedStore = { bucket: "fixture-bucket", region: "us-east-1", endpoint: "http://127.0.0.1:9", credentials: { provider: "s3", access_key_id: "fixture-selected-access", secret_access_key: "fixture-selected-secret", session_token: null } };
        const applied = await router.route(repoId, "task_apply", { repo_id: repoId, task_id: applyId, branch: apply.branch, base_ref: apply.base_ref, base_branch: "main", worktree_path: apply.path, __storage_auth: selectedStore }) as { applied: boolean; worktree_removed: boolean; cleanup_error: string };
        expect({ applied: applied.applied, removed: applied.worktree_removed, error: applied.cleanup_error, bytes: await readFile(path.join(repo, "applied.txt"), "utf8"), retained: fs.existsSync(apply.path) }).toEqual({ applied: true, removed: false, error: expect.stringContaining("locked"), bytes: "confirmed merge\n", retained: true });
        expect(await readFile(hookReceipt, "utf8")).toBe("verified");
        await fs.promises.unlink(path.join(hooks, "pre-merge-commit"));
        await run("git", ["worktree", "unlock", apply.path], { cwd: repo, env });
        await router.route(repoId, "task_discard", { repo_id: repoId, task_id: applyId, branch: apply.branch, worktree_path: apply.path });
        const destroyed = await router.route(repoId, "worktree_destroy", { repo_id: repoId, path: taskWorktrees[1].path, branch: taskWorktrees[1].branch, force: true });
        expect(destroyed).toEqual({ removed: true, path: taskWorktrees[1].path, branch_deleted: true, cleanup_error: null });
        await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "remote-cleanup-native.json"), JSON.stringify({ preserved: await readFile(path.join(bystander, "keep.txt"), "utf8"), removed, applied, destroyed }));
      } finally { await manager.disconnect("concurrent"); }
      const surviving = await router.sendToRemoteAgent("runtime", "indexer_status", {}) as { running: boolean };
      expect(surviving.running).toBe(true);
      // A real transport loss must defer the supported read until the new
      // sidecar has attached and completed its own capability handshake.
      progress("reconnect");
      for (const connection of connections) connection.end();
      await vi.waitFor(() => expect(manager.getStatus("runtime")).toBe("reconnecting"));
      const listing = await router.sendToRemoteAgent("runtime", "git_worktree_list", { repo_id: repoId });
      expect({ listing, status: manager.getStatus("runtime"), uploads }).toEqual({ listing: { worktrees: expect.arrayContaining([expect.objectContaining({ path: await realpath(worktree) })]) }, status: "connected", uploads: 2 });
      await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "ssh-native-runtime.json"), JSON.stringify({ downloads: requests, uploads, created, diff: comparison.diff, listing, status: manager.getStatus("runtime") }));
    } finally { progress("disconnect"); await manager.disconnect("runtime"); }
    await vi.waitFor(() => expect(children.size).toBe(0), { timeout: 10_000 });
    // Tracing's severity follows its timestamp. A WARN can contain the CLI's
    // serialized ERROR for an intentionally rejected request inside its field.
    const sidecarError = diagnostics.join("").replace(/\u001b\[[0-9;]*m/g, "").split(/\r?\n/).some(line => line.trim().split(/\s+/, 2)[1] === "ERROR");
    const ownership = { cleanExits: exits.every(code => code === 0), errors: sidecarError, cachesRemoved: cachePaths.every(cache => !fs.existsSync(cache)) };
    await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "task-workspace-diagnostics.json"), JSON.stringify({ ownership, diagnostics }));
    expect(ownership).toEqual({ cleanExits: true, errors: false, cachesRemoved: true });
    await writeFile(path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "indexer-concurrent-ssh.json"), JSON.stringify({ cachePaths, ...ownership }));
    progress("complete");
  }, 120_000);

});
