import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { parse } from 'yaml';

const require = createRequire(import.meta.url);
const { findFile, resolveFiles } = require('electron-updater/out/providers/Provider');
const { MacUpdater } = require('electron-updater/out/MacUpdater');
const FpmTarget = require('app-builder-lib/out/targets/FpmTarget').default;
const { convertIcon } = require('app-builder-lib/out/util/iconConverter');
const childProcess = require('node:child_process');
const script = fileURLToPath(new URL('./assemble-release.mjs', import.meta.url));
const version = '2.3.4';
const layouts = [
  ['mac-arm64', 'latest-mac.yml', ['mac-arm64.zip', 'mac-arm64.dmg']],
  ['mac-x64', 'latest-mac.yml', ['mac-x64.zip', 'mac-x64.dmg']],
  ['windows-x64', 'latest.yml', ['win-x64.exe']],
  ['linux-x64', 'latest-linux.yml', ['linux-x86_64.AppImage', 'linux-amd64.deb']],
];

test('Linux DEB configuration satisfies electron-builder package requirements', async () => {
  const config = parse(readFileSync(new URL('../electron-builder.yml', import.meta.url), 'utf8'));
  const desktop = JSON.parse(readFileSync(new URL('../package.json', import.meta.url)));
  const target = {
    options: config.linux,
    packager: { appInfo: { computePackageUrl: async () => desktop.repository.url, linuxPackageName: desktop.name }, info: { metadata: desktop } },
  };
  const metadata = await FpmTarget.prototype.computeFpmMetaInfoOptions.call(target);
  assert.ok(metadata.maintainer);
});

test('Windows installer icon can be generated from the configured image', async (t) => {
  const desktop = fileURLToPath(new URL('..', import.meta.url));
  const config = parse(readFileSync(path.join(desktop, 'electron-builder.yml'), 'utf8'));
  const output = mkdtempSync(path.join(tmpdir(), 'crab-desktop-icon-'));
  t.after(() => rmSync(output, { recursive: true, force: true }));
  for (const icon of [config.nsis.installerIcon, config.nsis.uninstallerIcon]) {
    if (icon) assert.ok(readFileSync(path.resolve(desktop, icon)).length > 0);
  }
  const result = await convertIcon({ sources: [config.win.icon], fallbackSources: [], roots: [path.join(desktop, 'build'), desktop], format: 'ico', outDir: output });
  assert.ok(result.icons?.some((icon) => icon.file.endsWith('.ico')));
});

function fixture(t, releaseVersion = version) {
  const version = releaseVersion;
  const root = mkdtempSync(path.join(tmpdir(), 'crab-desktop-release-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const input = path.join(root, 'input');
  const output = path.join(root, 'output');
  for (const [dir, channel, names] of layouts) {
    const folder = path.join(input, dir);
    mkdirSync(folder, { recursive: true });
    const files = names.map((suffix) => {
      const url = `Crab-${version}-${suffix}`;
      const data = Buffer.from(`installer: ${url}`);
      writeFileSync(path.join(folder, url), data);
      return { url, sha512: createHash('sha512').update(data).digest('base64'), size: data.length };
    });
    const prefix = version.includes('-') ? version.split('-')[1].split('.')[0] : 'latest';
    writeFileSync(path.join(folder, channel.replace('latest', prefix)), JSON.stringify({ version, files, path: files[0].url, sha512: files[0].sha512, releaseDate: '2026-01-01T00:00:00.000Z' }));
  }
  const runtimes = path.join(input, 'remote-runtimes'); mkdirSync(runtimes);
  for (const platform of ['darwin', 'linux']) for (const arch of ['arm64', 'x64']) {
    const prefix = `Crab-${version}-runtime-${platform}-${arch}`; const hashes = {};
    for (const name of ['crab', 'crab-desktop-agent']) {
      const data = Buffer.from(`${prefix}/${name}`); writeFileSync(path.join(runtimes, `${prefix}-${name}`), data); hashes[name] = createHash('sha256').update(data).digest('hex');
    }
    writeFileSync(path.join(runtimes, `${prefix}.json`), JSON.stringify({ version, platform, arch, hashes }));
  }
  const blockmap = `Crab-${version}-mac-arm64.zip.blockmap`;
  writeFileSync(path.join(input, 'mac-arm64', blockmap), 'blockmap');
  const assemble = () => spawnSync(process.execPath, [script, version, input, output], { encoding: 'utf8' });
  return { input, output, assemble, blockmap };
}

test('publishes alpha metadata without a stable update channel', (t) => {
  const version = '2.0.0-alpha.1';
  const { output, assemble } = fixture(t, version);
  const result = assemble();
  assert.equal(result.status, 0, result.stderr);
  for (const name of ['alpha-mac.yml', 'alpha.yml', 'alpha-linux.yml']) {
    assert.equal(parse(readFileSync(path.join(output, name), 'utf8')).version, version);
  }
  assert.throws(() => readFileSync(path.join(output, 'latest-mac.yml')), { code: 'ENOENT' });
});

test('assembles update channels, installers and verifiable checksums for all platforms', async (t) => {
  const { input, output, assemble, blockmap } = fixture(t);
  const result = assemble();
  assert.equal(result.status, 0, result.stderr);
  const mac = parse(readFileSync(path.join(output, 'latest-mac.yml'), 'utf8'));
  const base = new URL('https://github.com/crabbuild/crab/releases/download/v2.3.4/');
  const originalExec = childProcess.execFileSync;
  const originalArch = Object.getOwnPropertyDescriptor(process, 'arch');
  try {
    for (const [processArch, hardware, translated, expected] of [
      ['arm64', 'ARM64', false, 'arm64'],
      ['x64', 'x86_64', false, 'x64'],
      ['x64', 'ARM64', true, 'arm64'],
    ]) {
      Object.defineProperty(process, 'arch', { ...originalArch, value: processArch });
      childProcess.execFileSync = (command) => command === 'sysctl' ? `sysctl.proc_translated: ${Number(translated)}` : hardware;
      const updater = Object.create(MacUpdater.prototype);
      updater._logger = { info() {}, warn() {} };
      updater.executeDownload = ({ fileInfo }) => fileInfo;
      const provider = { resolveFiles: (info) => resolveFiles(info, base) };
      const selected = await updater.doDownloadUpdate({ updateInfoAndProvider: { info: mac, provider } });
      assert.equal(path.basename(selected.url.pathname), `Crab-${version}-mac-${expected}.zip`);
    }
  } finally {
    childProcess.execFileSync = originalExec;
    Object.defineProperty(process, 'arch', originalArch);
  }
  assert.equal(readFileSync(path.join(output, blockmap), 'utf8'), readFileSync(path.join(input, 'mac-arm64', blockmap), 'utf8'));
  for (const line of readFileSync(path.join(output, 'SHA256SUMS-desktop.txt'), 'utf8').trim().split('\n')) {
    const [hash, name] = line.split('  ');
    assert.equal(createHash('sha256').update(readFileSync(path.join(output, name))).digest('hex'), hash, name);
  }
  assert.equal(parse(readFileSync(path.join(output, 'latest.yml'), 'utf8')).version, version);
  const linux = parse(readFileSync(path.join(output, 'latest-linux.yml'), 'utf8'));
  const linuxFiles = resolveFiles(linux, new URL('https://github.com/crabbuild/crab/releases/download/v2.3.4/'));
  assert.equal(path.basename(findFile(linuxFiles, 'AppImage', ['deb']).url.pathname), `Crab-${version}-linux-x86_64.AppImage`);
  assert.equal(path.basename(findFile(linuxFiles, 'deb', ['AppImage']).url.pathname), `Crab-${version}-linux-amd64.deb`);
});

for (const [name, mutate, message] of [
  ['missing remote runtime', ({ input }) => rmSync(path.join(input, 'remote-runtimes', `Crab-${version}-runtime-linux-arm64-crab`)), /ENOENT/],
  ['corrupted remote runtime', ({ input }) => writeFileSync(path.join(input, 'remote-runtimes', `Crab-${version}-runtime-darwin-x64-crab`), 'corrupted'), /Remote runtime checksum mismatch/],
  ['corrupted installer', ({ input }) => writeFileSync(path.join(input, 'windows-x64', `Crab-${version}-win-x64.exe`), 'corrupt'), /does not match/],
  ['missing installer', ({ input }) => rmSync(path.join(input, 'linux-x64', `Crab-${version}-linux-amd64.deb`)), /ENOENT/],
  ['missing DEB update metadata', ({ input }) => {
    const file = path.join(input, 'linux-x64', 'latest-linux.yml');
    const data = JSON.parse(readFileSync(file)); data.files = data.files.slice(0, 1); writeFileSync(file, JSON.stringify(data));
  }, /Missing DEB update installer/],
  ['wrong version', ({ input }) => {
    const file = path.join(input, 'linux-x64', 'latest-linux.yml');
    const data = JSON.parse(readFileSync(file)); data.version = '2.3.5'; writeFileSync(file, JSON.stringify(data));
  }, /metadata or version/],
  ['conflicting macOS release policy', ({ input }) => {
    const file = path.join(input, 'mac-x64', 'latest-mac.yml');
    const data = JSON.parse(readFileSync(file)); data.stagingPercentage = 10; writeFileSync(file, JSON.stringify(data));
  }, /conflicting release policy/],
]) {
  test(`refuses ${name} before writing a release`, (t) => {
    const setup = fixture(t);
    mutate(setup);
    const result = setup.assemble();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, message);
    assert.throws(() => readFileSync(path.join(setup.output, 'latest-mac.yml')), { code: 'ENOENT' });
  });
}
