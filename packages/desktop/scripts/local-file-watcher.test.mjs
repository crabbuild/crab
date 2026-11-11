import { afterEach, beforeEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { LocalFileWatchers } = require('../dist-electron/local-file-watcher.cjs');
let root, service;
beforeEach(async () => {
  root = await mkdtemp(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), 'local-watch-'));
  service = new LocalFileWatchers();
});
afterEach(async () => { await service.stopAll(); await rm(root, { recursive: true, force: true }); });
function event() {
  let resolve;
  const promise = new Promise(yes => { resolve = yes; });
  return { promise, resolve };
}

function bounded(promise) {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Filesystem change was not delivered')), 5000); })]).finally(() => clearTimeout(timer));
}

test('independent leases share real changes and stopping one preserves the other', async () => {
  let first = 0, second = 0;
  let firstChanged = event(), secondChanged = event();
  const stopFirst = await service.watch(root, () => { first++; firstChanged.resolve(); });
  const stopSecond = await service.watch(root, () => { second++; secondChanged.resolve(); });
  await writeFile(path.join(root, 'first.txt'), 'FIRST');
  await bounded(Promise.all([firstChanged.promise, secondChanged.promise]));
  await stopFirst();
  const stoppedCount = first;
  secondChanged = event();
  await writeFile(path.join(root, 'second.txt'), 'SECOND');
  await bounded(secondChanged.promise);
  assert.equal(first, stoppedCount);
  assert.ok(second >= 2);
  await stopSecond();
});

test('a new lease cannot attach to an owner whose metadata scan is draining', async () => {
  const stop = await service.watch(root, () => {});
  const stopping = stop();
  const changed = event();
  const restarted = await service.watch(root, changed.resolve);
  await stopping;
  await writeFile(path.join(root, 'after-restart.txt'), 'RESTARTED');
  await bounded(changed.promise);
  await restarted();
});

test('shutdown joins both active roots and prevents delayed or future acquisition', async () => {
  const other = path.join(root, 'other'); await mkdir(other);
  await service.watch(root, () => {}); await service.watch(other, () => {});
  await service.stopAll();
  await assert.rejects(service.watch(root, () => {}), /shutting down/);
  const fresh = new LocalFileWatchers();
  const pending = fresh.watch(root, () => {});
  const rejected = assert.rejects(pending, /shutting down/);
  await fresh.stopAll(); await rejected;
});

test('invalid physical targets reject without poisoning another valid subscription', async () => {
  for (const target of ['relative', 'ssh://profile/repo', path.join(root, 'missing')]) await assert.rejects(service.watch(target, () => {}));
  const changed = event();
  const stop = await service.watch(root, changed.resolve);
  await writeFile(path.join(root, 'valid.txt'), 'VALID');
  await bounded(changed.promise);
  await stop();
});

test('multi-GiB content stays opaque while its metadata invalidates subscribers', async () => {
  const file = await open(path.join(root, 'large.bin'), 'w');
  try { await file.truncate(2 * 1024 ** 3); }
  finally { await file.close(); }
  let changed = event();
  const stop = await service.watch(root, () => changed.resolve());
  await bounded(changed.promise);
  changed = event();
  const update = await open(path.join(root, 'large.bin'), 'r+');
  try { await update.write(Buffer.from([42]), 0, 1, 123); }
  finally { await update.close(); }
  await bounded(changed.promise);
  await stop();
});

test('temporary root loss and repair invalidate even when the file metadata is identical', async () => {
  let changed = event();
  const stop = await service.watch(root, () => changed.resolve());
  await bounded(changed.promise);
  const moved = root + '-moved';
  let missing = false;
  try {
    changed = event(); await rename(root, moved); missing = true;
    await bounded(changed.promise);
    changed = event(); await rename(moved, root); missing = false;
    await bounded(changed.promise);
  } finally { if (missing) await rename(moved, root); await stop(); }
});
