import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { RemoteRuntimeStore } from "../electron/ssh-runtime";

const target = { platform: "linux", arch: "arm64" } as const;
const agent = Buffer.from("owned-sidecar"); const crab = Buffer.from("owned-matching-cli");
const digest = (data: Buffer) => createHash("sha256").update(data).digest("hex");
const metadata = { version: "2.0.0-alpha.1", ...target, hashes: { "crab-desktop-agent": digest(agent), crab: digest(crab) } };

describe("remote release runtime", () => {
  let directory: string;
  beforeEach(async () => { directory = await mkdtemp(path.join(tmpdir(), "crab-runtime-store-")); });
  afterEach(async () => { await rm(directory, { recursive: true, force: true }); });
  function options(packaged = true) { return { packaged, appPath: directory, version: metadata.version, cacheDirectory: path.join(directory, "cache") }; }
  const responses = () => vi.fn<typeof fetch>(async input => {
    const name = new URL(String(input)).pathname.split("/").at(-1)!;
    if (name.endsWith(".json")) return new Response(JSON.stringify(metadata));
    return new Response(Uint8Array.from(name.endsWith("-crab-desktop-agent") ? agent : crab));
  });
  async function stage(manifest = metadata) {
    const root = path.join(directory, "resources", "native", "linux-arm64"); await mkdir(root, { recursive: true });
    await writeFile(path.join(root, "runtime.json"), JSON.stringify(manifest));
    await writeFile(path.join(root, "crab-desktop-agent"), agent); await writeFile(path.join(root, "crab"), crab);
    return root;
  }
  it("uses the actual native development layout for the remote target", async () => {
    const root = await stage(); const request = responses();
    const runtime = await new RemoteRuntimeStore(options(false), request).resolve(target, new AbortController().signal);
    expect({ agent: runtime.agent, crab: runtime.crab, requests: request.mock.calls.length }).toEqual({ agent: path.join(root, "crab-desktop-agent"), crab: path.join(root, "crab"), requests: 0 });
  });
  it("rejects a native artifact for another release", async () => {
    await stage({ ...metadata, version: "1.0.0" });
    await expect(new RemoteRuntimeStore(options(false)).resolve(target, new AbortController().signal)).rejects.toThrow("does not match");
  });
  it("downloads and caches the complete matching release pair", async () => {
    const request = responses(); const store = new RemoteRuntimeStore(options(), request); const signal = new AbortController().signal;
    const runtime = await store.resolve(target, signal); const cached = await store.resolve(target, signal);
    expect({ files: [await readFile(runtime.agent), await readFile(runtime.crab)], cached, requests: request.mock.calls.map(call => String(call[0])) }).toEqual({ files: [agent, crab], cached: runtime, requests: [
      "https://github.com/crabbuild/crab/releases/download/v2.0.0-alpha.1/Crab-2.0.0-alpha.1-runtime-linux-arm64.json",
      "https://github.com/crabbuild/crab/releases/download/v2.0.0-alpha.1/Crab-2.0.0-alpha.1-runtime-linux-arm64-crab-desktop-agent",
      "https://github.com/crabbuild/crab/releases/download/v2.0.0-alpha.1/Crab-2.0.0-alpha.1-runtime-linux-arm64-crab",
    ] });
  });
  it("rejects corrupted downloaded bytes without publishing a partial cache", async () => {
    const request = responses(); request.mockResolvedValueOnce(new Response(JSON.stringify(metadata))).mockResolvedValueOnce(new Response("corrupted"));
    await expect(new RemoteRuntimeStore(options(), request).resolve(target, new AbortController().signal)).rejects.toThrow("checksum mismatch");
    expect(await readdir(path.join(directory, "cache", metadata.version))).toEqual([]);
  });
  it("fails closed if a completed cache is corrupted", async () => {
    const request = responses(); const store = new RemoteRuntimeStore(options(), request); const signal = new AbortController().signal;
    const runtime = await store.resolve(target, signal); await writeFile(runtime.crab, "corrupted");
    await expect(store.resolve(target, signal)).rejects.toThrow("checksum mismatch"); expect(request).toHaveBeenCalledTimes(3);
  });
  it("does not redownload a cache with a missing binary", async () => {
    const request = responses(); const store = new RemoteRuntimeStore(options(), request); const signal = new AbortController().signal;
    const runtime = await store.resolve(target, signal); await rm(runtime.crab);
    await expect(store.resolve(target, signal)).rejects.toMatchObject({ cause: { code: "ENOENT" } }); expect(request).toHaveBeenCalledTimes(3);
  });
  it("rejects redirects outside official HTTPS release hosts", async () => {
    const request = responses(); request.mockResolvedValueOnce(new Response(null, { status: 302, headers: { location: "https://untrusted.invalid/runtime" } }));
    await expect(new RemoteRuntimeStore(options(), request).resolve(target, new AbortController().signal)).rejects.toThrow("official release hosts"); expect(request).toHaveBeenCalledTimes(1);
  });
  it("follows the official release asset redirect", async () => {
    const request = responses(); request.mockResolvedValueOnce(new Response(null, { status: 302, headers: { location: "https://release-assets.githubusercontent.com/owned.json" } }));
    await new RemoteRuntimeStore(options(), request).resolve(target, new AbortController().signal);
    expect(request.mock.calls[1][0].toString()).toBe("https://release-assets.githubusercontent.com/owned.json");
  });
  it("cancels an incomplete download without installing it", async () => {
    const owner = new AbortController(); const request = responses();
    request.mockImplementationOnce(async (_input, init) => {
      const body = new ReadableStream({ start(controller) { init?.signal?.addEventListener("abort", () => controller.error(init.signal?.reason), { once: true }); } });
      return new Response(body);
    });
    const resolving = new RemoteRuntimeStore(options(), request).resolve(target, owner.signal);
    await vi.waitFor(() => expect(request).toHaveBeenCalled()); owner.abort(new Error("Connection retired"));
    await expect(resolving).rejects.toThrow("Connection retired"); expect(await readdir(path.join(directory, "cache", metadata.version))).toEqual([]);
  });
  it("shares one verified download between concurrent connections", async () => {
    const request = responses(); const store = new RemoteRuntimeStore(options(), request);
    const [first, second] = await Promise.all([store.resolve(target, new AbortController().signal), store.resolve(target, new AbortController().signal)]);
    expect({ first, second, requests: request.mock.calls.length }).toEqual({ first, second: first, requests: 3 });
  });

  it("lets one connection cancel while another still needs the same download", async () => {
    let publish!: (value: Response) => void;
    const request = responses(); request.mockImplementationOnce(async () => new Promise<Response>(resolve => { publish = resolve; }));
    const store = new RemoteRuntimeStore(options(), request); const owner = new AbortController();
    const first = store.resolve(target, owner.signal).catch((error: unknown) => error);
    const second = store.resolve(target, new AbortController().signal);
    await vi.waitFor(() => expect(request).toHaveBeenCalled()); owner.abort(new Error("First profile retired"));
    expect(await first).toMatchObject({ message: "First profile retired" });
    publish(new Response(JSON.stringify(metadata))); const runtime = await second;
    expect({ requests: request.mock.calls.length, bytes: await readFile(runtime.crab) }).toEqual({ requests: 3, bytes: crab });
  });

});
