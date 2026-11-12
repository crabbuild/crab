import assert from "node:assert/strict";
import { spawn, execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { chmod, copyFile, mkdir, mkdtemp, open, readFile, readdir, realpath, rename, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { createInterface } from "node:readline";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { setTimeout as pause } from "node:timers/promises";
import { promisify } from "node:util";
import { createDeflate } from "node:zlib";

const execute = promisify(execFile);
const packed = process.argv[3] === "--packed";
const delta = process.argv[3] === "--delta"
  ? JSON.parse(await readFile(process.argv[4], "utf8")) : undefined;
if ((delta ? process.argv.length !== 5 : process.argv.length > 4) || (process.argv[3] && !packed && !delta)) {
  throw new Error("Usage: qualify-git-ranges.mjs [native-directory] [--packed | --delta <manifest>]");
}
const size = delta ? delta.logicalBytes : 2 * 1024 ** 3;
const head = delta ? Buffer.from(delta.head, "base64") : Buffer.from("verified-git-head\n");
const tail = delta ? Buffer.from(delta.tail, "base64") : Buffer.from("verified-git-tail\n");
const fixtureDigest = delta ? delta.sha256 : "515a224ca4c51ce3dbabf555d1d1da8d2fceeccfc9d0b24903b74dba1f6a2731";
const pattern = delta ? Buffer.alloc(64 * 1024)
  : Buffer.concat(Array.from({ length: 2048 }, (_, i) => createHash("sha256").update(`crab-dense-git-block-${i}`).digest()));
const platform = { darwin: "mac", linux: "linux", win32: "win" }[process.platform];
const directory = path.resolve(process.argv[2] ?? `resources/native/${platform}-${process.arch}`);
const runtime = JSON.parse(await readFile(path.join(directory, "runtime.json"), "utf8"));
assert.equal(runtime.platform, process.platform);
assert.equal(runtime.arch, process.arch);
const extension = process.platform === "win32" ? ".exe" : "";
const agent = path.join(directory, `crab-desktop-agent${extension}`), crab = path.join(directory, `crab${extension}`);

async function digest(file) {
  const hash = createHash("sha256");
  for await (const bytes of createReadStream(file)) hash.update(bytes);
  return hash.digest("hex");
}
for (const binary of [agent, crab]) assert.equal(await digest(binary), runtime.hashes[path.basename(binary)]);
const target = process.env.CARGO_TARGET_DIR;
if (!target || !path.isAbsolute(target)) throw new Error("Set an absolute per-checkout CARGO_TARGET_DIR for the owned fixture");
const targetRoot = await realpath(target);
if (delta) {
  assert.equal(delta.schema, "crab.desktop.git-delta-fixture");
  assert.equal(size, 512 * 1024 ** 2);
  assert.equal(path.dirname(delta.evidenceDirectory), targetRoot);
  assert.match(path.basename(delta.evidenceDirectory), /^git-range-qualification-/);
  assert.equal(delta.source, path.join(delta.evidenceDirectory, "source"));
  assert.ok(delta.depth > 0);
}
const evidence = delta ? delta.evidenceDirectory : await mkdtemp(path.join(targetRoot, "git-range-qualification-"));
const source = path.join(evidence, "source");
if (!delta) {
  await mkdir(source);
  await writeFile(path.join(evidence, "gitconfig"), "");
}
const env = { ...process.env, CRAB_BIN: crab, CRAB_CACHE_DIR: path.join(evidence, "cache"),
  GIT_CONFIG_GLOBAL: path.join(evidence, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
for (const name of ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"]) delete env[name];
const git = async args => (await execute("git", args, { cwd: source, env })).stdout.trim();
const report = { schema: "crab.desktop.git-range-qualification", version: 1, runtime, fixtureDigest,
  logicalBytes: size, fixture: delta ? "native Git verified 512 MiB packed delta; preparation excluded from measured read tree; OS cache warmed by setup"
    : `dense repeating 64 KiB deterministic content; level-0 ${packed ? "single full-blob pack" : "zlib loose object"}; OS cache warmed by setup`,
  delta,
  evidenceDirectory: evidence, ranges: [], verified: false, closed: false };
const repo = "git-range-qualification";
let child, exit, rpc, captured, object, swapped = false, failure;
const backup = path.join(evidence, "original-object");
const cleanupErrors = [];

async function storeBlob(file) {
  if (packed) {
    const packs = path.join(source, ".git/objects/pack"), before = new Set(await readdir(packs));
    const oid = await git(["-c", "core.compression=0", "-c", "core.bigFileThreshold=1", "hash-object", "--no-filters", "-w", file]);
    const added = (await readdir(packs)).filter(name => name.endsWith(".pack") && !before.has(name));
    assert.equal(added.length, 1, "Fixture must create one physical pack");
    const object = path.join(packs, added[0]), descriptor = await open(object, "r");
    try {
      const header = Buffer.alloc(13);
      assert.equal((await descriptor.read(header, 0, header.length, 0)).bytesRead, header.length);
      assert.equal(header.subarray(0, 4).toString(), "PACK");
      assert.equal(header.readUInt32BE(4), 2);
      assert.equal(header.readUInt32BE(8), 1);
      assert.equal((header[12] >> 4) & 7, 3, "Stored entry must be a full blob, not a delta");
    } finally { await descriptor.close(); }
    await assert.rejects(stat(path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2))), error => error.code === "ENOENT");
    return { oid, object };
  }
  // Large hash-object writes can create packs. Keep this fault fixture loose
  // so swapping one captured object's bytes cannot be intercepted by its pack.
  // Format: https://git-scm.com/book/en/v2/Git-Internals-Git-Objects#_object_storage
  const header = Buffer.from(`blob ${size}\0`), hash = createHash("sha1").update(header);
  const temporary = path.join(evidence, `object-${randomUUID()}`);
  const input = Readable.from((async function* () {
    yield header;
    for await (const bytes of createReadStream(file)) { hash.update(bytes); yield bytes; }
  })(), { objectMode: false });
  // Sparse or highly compressed objects hide Git's whole-loose-object mapping.
  // Stored zlib blocks exercise the physical 2 GiB source without relying on
  // filesystem sparse allocation or compressor behavior on a particular runner.
  await pipeline(input, createDeflate({ level: 0 }), createWriteStream(temporary, { flags: "wx", mode: 0o600 }));
  const oid = hash.digest("hex"), object = path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2));
  await mkdir(path.dirname(object), { recursive: true });
  await rename(temporary, object);
  return { oid, object };
}

async function repair() {
  if (!swapped) return;
  await rm(object, { force: true });
  await rename(backup, object);
  swapped = false;
}

try {
  const file = path.join(source, "large.bin");
  if (!delta) {
    await git(["init", "--template=", "--object-format=sha1", "--initial-branch=main"]);
    const descriptor = await open(file, "w");
    try {
      const block = Buffer.concat(Array(16).fill(pattern));
      for (let offset = 0; offset < size; offset += block.length) {
        let written = 0;
        while (written < block.length) {
          const result = await descriptor.write(block, written, block.length - written, offset + written);
          assert.ok(result.bytesWritten > 0, "Dense fixture write must make progress");
          written += result.bytesWritten;
        }
      }
      assert.equal((await descriptor.write(head, 0, head.length, 0)).bytesWritten, head.length);
      assert.equal((await descriptor.write(tail, 0, tail.length, size - tail.length)).bytesWritten, tail.length);
    } finally { await descriptor.close(); }
  }
  assert.equal(await digest(file), fixtureDigest);
  const stored = delta ? { oid: delta.oid, object: delta.object } : await storeBlob(file), { oid } = stored;
  if (!delta) {
    assert.equal(oid, "5fa9fa26c5fb2dbcb4dacd5d02debc475955a204");
    await git(["update-index", "--add", "--cacheinfo", "100644", oid, "large.bin"]);
  }
  const index = await readFile(path.join(source, ".git/index"));
  object = stored.object;
  report.oid = oid;
  report.storedBytes = (await stat(object)).size;
  const log = await open(path.join(evidence, "agent.log"), "w");
  child = spawn(agent, [], { cwd: source, env, stdio: ["pipe", "pipe", log.fd] });
  await log.close();
  const pending = new Map();
  let next = 0, transportError;
  const fail = error => {
    transportError ??= error;
    for (const entry of pending.values()) { clearTimeout(entry.timer); entry.reject(error); }
    pending.clear();
  };
  child.stdin.on("error", fail);
  const lines = createInterface({ input: child.stdout });
  lines.on("line", line => {
    try {
      const frame = JSON.parse(line), entry = pending.get(frame.id);
      if (!entry) return;
      pending.delete(frame.id); clearTimeout(entry.timer);
      if (frame.error) entry.reject(Object.assign(new Error(frame.error.message), { code: frame.error.code, data: frame.error.data }));
      else entry.resolve(frame.result);
    } catch (error) { fail(error); }
  });
  exit = new Promise((resolve, reject) => {
    child.once("error", error => { fail(error); reject(error); });
    child.once("close", (code, signal) => {
      lines.close(); fail(new Error(`Agent exited (${code}, ${signal})`));
      if (code === 0) resolve(); else reject(transportError);
    });
  });
  void exit.catch(() => {});
  rpc = (method, params) => new Promise((resolve, reject) => {
    if (transportError) { reject(transportError); return; }
    const id = ++next;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} response timed out`)); }, 60_000);
    pending.set(id, { resolve, reject, timer });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
  assert.equal((await rpc("ping", {})).pong, runtime.version);
  await rpc("open_local", { repo_id: repo, path: source });
  captured = await rpc("file_open", { repo_id: repo, path: "large.bin", rev: "" });
  assert.equal(captured.size, size);
  assert.equal(captured.representation, "git_blob");
  const read = async offset => {
    const started = performance.now();
    const result = await rpc("read_range", { repo_id: repo, file_id: captured.file_id, offset, length: 4096 });
    const expected = Buffer.alloc(4096);
    for (let i = 0; i < expected.length; i++) expected[i] = pattern[(offset + i) % pattern.length];
    for (const [position, marker] of [[0, head], [size - tail.length, tail]]) {
      const from = Math.max(offset, position), to = Math.min(offset + expected.length, position + marker.length);
      if (from < to) marker.copy(expected, from - offset, from - position, to - position);
    }
    assert.equal(result.offset, offset); assert.equal(result.length, expected.length); assert.equal(result.file_size, size);
    assert.deepEqual(Buffer.from(result.bytes_b64, "base64"), expected);
    const phase = report.ranges.length === 0 ? "first capture read" : "subsequent capture read";
    report.ranges.push({ offset, length: expected.length, elapsedMs: performance.now() - started, phase });
  };
  for (const offset of [0, size / 2 - 17, size - 4096]) await read(offset);

  // Loose/full-pack profiles retain their unread-suffix fault and repair checks.
  // The separately prepared delta profile shares exact/cancel/close assertions
  // without running Git's full-memory fixture builder inside the RSS boundary.
  if (!delta) {
    const changed = await open(file, "r+");
    try {
      await changed.write(Buffer.from([0]), 0, 1, size - 1);
      const replacement = await storeBlob(file);
      await changed.write(tail.subarray(-1), 0, 1, size - 1);
      await rename(object, backup); swapped = true;
      await copyFile(replacement.object, object);
      if (packed) {
        // Git compares the stored trailer with its index at open; keep that
        // relationship while substituting valid zlib bytes. The captured blob
        // checksum, not pack naming or a successful Git exit, must reject them.
        await chmod(object, 0o600);
        const originalPack = await open(backup, "r"), replacedPack = await open(object, "r+");
        try {
          const checksum = Buffer.alloc(20), length = (await originalPack.stat()).size;
          assert.equal((await replacedPack.stat()).size, length);
          assert.equal((await originalPack.read(checksum, 0, 20, length - 20)).bytesRead, 20);
          assert.equal((await replacedPack.write(checksum, 0, 20, length - 20)).bytesWritten, 20);
        } finally { await originalPack.close(); await replacedPack.close(); }
      }
    } finally { await changed.close(); }
    await assert.rejects(rpc("read_range", { repo_id: repo, file_id: captured.file_id, offset: 0, length: 4096 }),
      error => error.code === 1003 && /checksum differs/.test(error.message));
    report.corruptSuffixRejected = true;
    await repair();
    await read(size - 4096);
  }
  const operation = randomUUID();
  const stopped = assert.rejects(rpc("read_range", { repo_id: repo, file_id: captured.file_id, offset: 0, length: 4096, op_id: operation }),
    error => error.code === 1010 && error.data?.kind === "op_cancelled" && error.data.op_id === operation);
  await pause(50);
  assert.equal((await rpc("rpc/cancel", { op_id: operation })).ok, true);
  await stopped;
  report.cancellationAcknowledged = true;
  await read(0);
  assert.deepEqual(await readFile(path.join(source, ".git/index")), index);
  assert.equal(await digest(file), fixtureDigest);
  report.verified = true;
} catch (error) { failure = error; }
finally {
  try { await repair(); } catch (error) { cleanupErrors.push(error); }
  if (child) {
    try {
      if (captured) assert.equal((await rpc("file_close", { repo_id: repo, file_id: captured.file_id })).closed, captured.file_id);
      assert.equal((await rpc("close", { repo_id: repo })).closed, repo);
    } catch (error) { cleanupErrors.push(error); }
    child.stdin.end();
    try { await exit; report.closed = true; } catch (error) { cleanupErrors.push(error); }
  }
  if (!child || report.closed) {
    try { await rm(source, { recursive: true }); await rm(path.join(evidence, "cache"), { recursive: true, force: true }); }
    catch (error) { cleanupErrors.push(error); }
  }
  report.errors = [...(failure ? [failure] : []), ...cleanupErrors].map(error => String(error));
  await writeFile(path.join(evidence, "result.json"), JSON.stringify(report, null, 2) + "\n");
  console.log(JSON.stringify(report));
}
if (failure || cleanupErrors.length) throw new AggregateError([...(failure ? [failure] : []), ...cleanupErrors], "Git range qualification failed");
