import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const bumpScript = fileURLToPath(new URL('../../../crab/scripts/release/bump-version.sh', import.meta.url));
const manifests = [
  ['crab', 'crab'],
  ['crates/crab-auth-server', 'crab-auth-server'],
  ['crates/crab-cache-server', 'crab-cache-server'],
  ['packages/desktop/agent', 'crab-desktop-agent'],
];

function fixture(t) {
  const root = mkdtempSync(path.join(tmpdir(), 'crab-version-bump-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(path.join(root, 'crab/scripts/release'), { recursive: true });
  copyFileSync(bumpScript, path.join(root, 'crab/scripts/release/bump-version.sh'));
  writeFileSync(path.join(root, 'Cargo.toml'), `[workspace]\nresolver = "3"\nmembers = ${JSON.stringify(manifests.map(([dir]) => dir))}\n\n[workspace.dependencies]\ncrab-auth-server = { version = "=2.0.0", path = "crates/crab-auth-server" }\ncrab-cache-server = { version = "=2.0.0", path = "crates/crab-cache-server" }\n`);
  for (const [dir, name] of manifests) {
    mkdirSync(path.join(root, dir, 'src'), { recursive: true });
    writeFileSync(path.join(root, dir, 'Cargo.toml'), `[package]\nname = "${name}"\nversion = "2.0.0"\nedition = "2024"\n`);
    writeFileSync(path.join(root, dir, 'src/lib.rs'), '');
  }
  const desktop = path.join(root, 'packages/desktop');
  writeFileSync(path.join(desktop, 'package.json'), JSON.stringify({ name: 'crab-desktop', version: '2.0.0', private: true }, null, 2));
  writeFileSync(path.join(desktop, 'package-lock.json'), JSON.stringify({ name: 'crab-desktop', version: '2.0.0', lockfileVersion: 3, packages: { '': { name: 'crab-desktop', version: '2.0.0' } } }, null, 2));
  const env = { ...process.env, CARGO_NET_OFFLINE: 'true' };
  const lock = spawnSync('cargo', ['generate-lockfile', '--offline'], { cwd: root, env, encoding: 'utf8' });
  assert.equal(lock.status, 0, lock.stderr);
  const files = ['Cargo.toml', 'Cargo.lock', ...manifests.map(([dir]) => `${dir}/Cargo.toml`), 'packages/desktop/package.json', 'packages/desktop/package-lock.json'];
  return { root, env, files, bump: (environment = env) => spawnSync('bash', ['crab/scripts/release/bump-version.sh', 'patch'], { cwd: root, env: environment, encoding: 'utf8' }) };
}

test('bumps desktop and core manifests and both lockfiles together', { skip: process.platform === 'win32' }, (t) => {
  const { root, files, bump } = fixture(t);
  const result = bump();
  assert.equal(result.status, 0, result.stderr);
  const workspace = readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  for (const name of ['crab-auth-server', 'crab-cache-server']) assert.match(workspace, new RegExp(`${name} = \\{ version = "=2\\.0\\.1"`));
  for (const file of files.slice(2, -2)) assert.match(readFileSync(path.join(root, file), 'utf8'), /version = "2\.0\.1"/);
  const desktop = JSON.parse(readFileSync(path.join(root, 'packages/desktop/package.json')));
  const lock = JSON.parse(readFileSync(path.join(root, 'packages/desktop/package-lock.json')));
  assert.deepEqual([desktop.version, lock.version, lock.packages[''].version], ['2.0.1', '2.0.1', '2.0.1']);
  const cargoLock = readFileSync(path.join(root, 'Cargo.lock'), 'utf8');
  for (const [, name] of manifests) assert.match(cargoLock, new RegExp(`name = "${name}"\\nversion = "2\\.0\\.1"`));
});

test('restores every product manifest and lockfile if the desktop bump fails', { skip: process.platform === 'win32' }, (t) => {
  const { root, env, files, bump } = fixture(t);
  const original = files.map((file) => readFileSync(path.join(root, file), 'utf8'));
  const bin = path.join(root, 'fake-bin');
  mkdirSync(bin);
  writeFileSync(path.join(bin, 'npm'), '#!/bin/sh\nexit 7\n');
  chmodSync(path.join(bin, 'npm'), 0o755);
  const result = bump({ ...env, PATH: `${bin}${path.delimiter}${env.PATH}` });
  assert.notEqual(result.status, 0);
  assert.deepEqual(files.map((file) => readFileSync(path.join(root, file), 'utf8')), original);
});
