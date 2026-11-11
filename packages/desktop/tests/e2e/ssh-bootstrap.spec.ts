import { test, expect } from "@playwright/test";
import { createReadStream } from "node:fs";
import { Readable } from "node:stream";
import type { RemoteRuntimeStore } from "../../electron/ssh-runtime";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { SshSessionManager, SshProfile } from "../../electron/ssh-manager";
import type { RpcRouter } from "../../electron/ssh-router";
import type { AgentDeployer } from "../../electron/ssh-deployer";
import type { CapabilityNegotiator } from "../../electron/ssh-capabilities";
import { packagedExecutable, packagedModule, unboundStorage } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";

test("SSH bootstrap deploys the real native pair, reads repositories and drains independent sessions", async () => {
  test.skip(process.platform === "win32", "The remote execution fixture needs a native POSIX host; Windows client qualification remains separate");
  test.setTimeout(180_000);
  const executable = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executable), "../Resources") : path.join(path.dirname(executable), "resources");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-bootstrap-"));
  const crab = path.join(resources, "agent/crab");
  const { SshSessionManager: Manager } = packagedModule<{ SshSessionManager: typeof SshSessionManager }>(resources, "ssh-manager");
  const { RpcRouter: Router } = packagedModule<{ RpcRouter: typeof RpcRouter }>(resources, "ssh-router");
  const { AgentDeployer: Deployer } = packagedModule<{ AgentDeployer: typeof AgentDeployer }>(resources, "ssh-deployer");
  const { CapabilityNegotiator: Negotiator } = packagedModule<{ CapabilityNegotiator: typeof CapabilityNegotiator }>(resources, "ssh-capabilities");
  writeFileSync(path.join(root, "gitconfig"), "");
  const environment: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: "/not-the-bundled-crab", CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete environment.GIT_DIR; delete environment.GIT_WORK_TREE; delete environment.GIT_INDEX_FILE;
  const fixture = await sshFixture(root, environment);
  const manager = new Manager(fixture.verify);
  const storage = unboundStorage(resources, path.join(root, "connections.enc"), (keys, state) => manager.storageIdentityChanged(keys, state));
  const router = new Router(manager, { request: async () => { throw new Error("Remote operation reached the local agent"); } }, storage);
  const capabilities = new Negotiator(router);
  const version = JSON.parse(readFileSync(path.join(resources, "agent/runtime.json"), "utf8")).version as string;
  const { RemoteRuntimeStore: RuntimeStore } = packagedModule<{ RemoteRuntimeStore: typeof RemoteRuntimeStore }>(resources, "ssh-runtime");
  const runtime = new RuntimeStore({ packaged: true, appPath: resources, version, cacheDirectory: path.join(root, "runtimes") }, async input => {
    const name = path.basename(new URL(String(input)).pathname);
    return new Response(Readable.toWeb(createReadStream(path.resolve("release/remote-runtime", name))) as unknown as ReadableStream<Uint8Array>);
  });
  manager.setAgentDependencies({ createDeployer: () => new Deployer(runtime.resolve.bind(runtime)), capabilityNegotiator: capabilities, rpcRouter: router });
  const profile = (id: string): SshProfile => ({ id, label: "Bootstrap fixture", host: "127.0.0.1", port: fixture.port, username: "fixture", authMethod: { type: "key", keyPath: fixture.keyPath }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: false, autoDeployAgent: true, tags: [], portForwards: [], createdAt: Date.now() });
  const first = profile(randomUUID()), second = profile(randomUUID());
  first.autoReconnect = true;
  let stage = "setup";
  let failure: unknown;
  try {
    const source = path.join(fixture.home, "source '🦀'"); mkdirSync(source);
    const setupEnv = { ...environment, CRAB_BIN: crab, PATH: `${path.dirname(crab)}${path.delimiter}${environment.PATH ?? ""}` };
    const git = (args: string[]) => execFileSync("git", args, { cwd: source, env: setupEnv });
    git(["init", "--initial-branch=main"]); mkdirSync(path.join(source, ".crab"));
    writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks"))}\n`);
    for (const kind of ["crab", "lfs"]) { git(["config", `filter.${kind}.process`, "crab filter-process"]); git(["config", `filter.${kind}.required`, "true"]); }
    writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
    writeFileSync(path.join(source, ".gitattributes"), "dataset.bin filter=crab -text\nlfs.bin filter=lfs -text\n");
    const bytes = Buffer.alloc(4 * 1024 ** 2); for (let i = 0; i < bytes.length; i++) bytes[i] = (i * 37 + (i >>> 12)) & 255;
    const pointers = new Map<string, Buffer>();
    for (const file of ["dataset.bin", "lfs.bin"]) {
      writeFileSync(path.join(source, file), bytes); git(["add", file]);
      const pointer = git(["show", `:${file}`]); expect(pointer.length).toBeLessThan(1024); pointers.set(file, pointer); writeFileSync(path.join(source, file), pointer);
    }
    git(["add", ".gitattributes", ".gitignore"]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Native SSH deployment fixture"]);
    await manager.connect(first);
    expect(capabilities.isSupported(first.id, "open_local", "file_open", "read_range", "file_materialize")).toBe(true);
    expect(fixture.uploadedBytes).toBeGreaterThan(0);
    const firstUpload = fixture.uploadedBytes;
    await manager.connect(second);
    expect(fixture.uploadedBytes).toBe(firstUpload);
    const left = await manager.openRepository(first.id, source), right = await manager.openRepository(second.id, source);
    expect(left).toMatchObject({ repo_id: `ssh://${first.id}${source}`, is_remote: true });
    expect(right).toMatchObject({ repo_id: `ssh://${second.id}${source}`, is_remote: true });
    expect(left.repo_id).not.toBe(right.repo_id);
    for (const file of ["dataset.bin", "lfs.bin"]) {
      const repo_id = left.repo_id as string;
      const captured = await router.route(repo_id, "file_open", { repo_id, path: file }) as { file_id: string };
      const range = await router.route(repo_id, "read_range", { repo_id, file_id: captured.file_id, offset: bytes.length - 31, length: 31 }) as { bytes_b64: string };
      expect(Buffer.from(range.bytes_b64, "base64")).toEqual(bytes.subarray(-31));
      await router.route(repo_id, "file_materialize", { repo_id, file_id: captured.file_id });
      expect(readFileSync(path.join(source, file))).toEqual(bytes); expect(git(["show", `:${file}`])).toEqual(pointers.get(file));
      await router.route(repo_id, "file_close", { repo_id, file_id: captured.file_id });
    }
    stage = "first disconnect";
    await manager.disconnect(first.id);
    expect(await router.route(right.repo_id as string, "ping", {})).toMatchObject({ pong: version });
    stage = "explicit reconnect";
    await manager.connect(first);
    expect(fixture.uploadedBytes).toBe(firstUpload);
    expect(await router.route(left.repo_id as string, "git_status_porcelain", { repo_id: left.repo_id })).toBeDefined();
    stage = "automatic reconnect";
    const previous = manager.getConnection(first.id)!.channels.agent;
    manager.getConnection(first.id)!.client.end();
    await expect.poll(() => manager.getStatus(first.id)).toBe("reconnecting");
    const replayed = router.route(left.repo_id as string, "git_status_porcelain", { repo_id: left.repo_id });
    await expect.poll(() => manager.getStatus(first.id), { timeout: 30_000 }).toBe("connected");
    expect(await replayed).toMatchObject({ entries: expect.any(Array) });
    expect(manager.getConnection(first.id)!.channels.agent).not.toBe(previous);
    expect(fixture.uploadedBytes).toBe(firstUpload);
    expect(await router.route(right.repo_id as string, "ping", {})).toMatchObject({ pong: version });
    stage = "final disconnect";
    await manager.disconnect(first.id); await manager.disconnect(second.id);
    expect(fixture.commands.size).toBe(0); expect(fixture.openFiles).toBe(0);
    expect(readdirSync(path.join(fixture.home, ".crab/desktop/runtimes")).filter(name => name.startsWith(".index-") || name.startsWith(".upload-"))).toEqual([]);
    writeFileSync(test.info().outputPath("ssh-bootstrap-proof.json"), JSON.stringify({ bytesPerFile: bytes.length, formats: ["crab", "lfs"], deployedBytes: firstUpload, secondProfileUploadBytes: 0, reconnectedUploadBytes: 0, nativeSessionsDrained: true, independentProfiles: true, repositoryReopened: true, automaticReconnect: true, metadataQueueReplayed: true, installedRuntimes: readdirSync(path.join(fixture.home, ".crab/desktop/runtimes")) }, null, 2));
  } catch (error) { failure = error; }
  finally {
    const outcomes = await Promise.allSettled([manager.disconnect(first.id), manager.disconnect(second.id)]);
    const failures: unknown[] = failure ? [failure] : [];
    for (const outcome of outcomes) if (outcome.status === "rejected") failures.push(outcome.reason);
    try { await fixture.close(); } catch (error) { failures.push(error); }
    finally { rmSync(root, { recursive: true, force: true }); }
    writeFileSync(test.info().outputPath("bootstrap-outcome.json"), JSON.stringify({ stage, failures: failures.map(error => error instanceof Error ? { name: error.name, message: error.message, stack: error.stack } : String(error)) }, null, 2));
    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) throw new AggregateError(failures, "SSH bootstrap and cleanup failed");
  }
});
