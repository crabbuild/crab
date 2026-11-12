import assert from "node:assert/strict";
import { spawn, execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { copyFile, mkdir, mkdtemp, open, readFile, realpath, rename, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { createInterface } from "node:readline";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { setTimeout as pause } from "node:timers/promises";
import { promisify } from "node:util";
import { createDeflate } from "node:zlib";

const execute = promisify(execFile);
const size = 2 * 1024 ** 3;
const head = Buffer.from("verified-git-head\n"), tail = Buffer.from("verified-git-tail\n");
const fixtureDigest = "4f7eb1a4a45d43b4d7e36edd9b8bdbfa3a807d9b5578d1cc467d20cf99913596";
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
const evidence = await mkdtemp(path.join(await realpath(target), "git-range-qualification-"));
const source = path.join(evidence, "source");
await mkdir(source);
await writeFile(path.join(evidence, "gitconfig"), "");
const env = { ...process.env, CRAB_BIN: crab, CRAB_CACHE_DIR: path.join(evidence, "cache"),
  GIT_CONFIG_GLOBAL: path.join(evidence, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
for (const name of ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"]) delete env[name];
const git = async args => (await execute("git", args, { cwd: source, env })).stdout.trim();
const report = { schema: "crab.desktop.git-range-qualification", version: 1, runtime, fixtureDigest,
  logicalBytes: size, fixture: "sparse file; highly compressible Git object; OS cache warmed by setup",
  evidenceDirectory: evidence, ranges: [], verified: false, closed: false };
const repo = "git-range-qualification";
let child, exit, rpc, captured, object, swapped = false, failure;
const backup = path.join(evidence, "original-object");
const cleanupErrors = [];

async function storeBlob(file) {
  // Large hash-object writes can create packs. Keep this fault fixture loose
  // so swapping one captured object's bytes cannot be intercepted by its pack.
  // Format: https://git-scm.com/book/en/v2/Git-Internals-Git-Objects#_object_storage
  const header = Buffer.from(`blob ${size}\0`), hash = createHash("sha1").update(header);
  const temporary = path.join(evidence, `object-${randomUUID()}`);
  const input = Readable.from((async function* () {
    yield header;
    for await (const bytes of createReadStream(file)) { hash.update(bytes); yield bytes; }
  })(), { objectMode: false });
  await pipeline(input, createDeflate(), createWriteStream(temporary, { flags: "wx", mode: 0o600 }));
  const oid = hash.digest("hex"), object = path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2));
  await mkdir(path.dirname(object), { recursive: true });
  await rename(temporary, object);
  return oid;
}

async function repair() {
  if (!swapped) return;
  await rm(object, { force: true });
  await rename(backup, object);
  swapped = false;
}

try {
  await git(["init", "--template=", "--object-format=sha1", "--initial-branch=main"]);
  const file = path.join(source, "large.bin");
  const descriptor = await open(file, "w");
  try {
    await descriptor.truncate(size);
    assert.equal((await descriptor.write(head, 0, head.length, 0)).bytesWritten, head.length);
    assert.equal((await descriptor.write(tail, 0, tail.length, size - tail.length)).bytesWritten, tail.length);
  } finally { await descriptor.close(); }
  assert.equal(await digest(file), fixtureDigest);
  const oid = await storeBlob(file);
  assert.equal(oid, "5e604980012d82c094e926b9814d98699ad80c1f");
  await git(["update-index", "--add", "--cacheinfo", "100644", oid, "large.bin"]);
  const index = await readFile(path.join(source, ".git/index"));
  object = path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2));
  report.oid = oid;
  report.compressedBytes = (await stat(object)).size;
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

  const changed = await open(file, "r+");
  try {
    await changed.write(Buffer.from([0]), 0, 1, size - 1);
    const replacement = await storeBlob(file);
    await changed.write(tail.subarray(-1), 0, 1, size - 1);
    await rename(object, backup); swapped = true;
    await copyFile(path.join(source, ".git/objects", replacement.slice(0, 2), replacement.slice(2)), object);
  } finally { await changed.close(); }
  await assert.rejects(rpc("read_range", { repo_id: repo, file_id: captured.file_id, offset: 0, length: 4096 }),
    error => error.code === 1003 && /checksum differs/.test(error.message));
  report.corruptSuffixRejected = true;
  await repair();
  await read(size - 4096);
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
