import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { copyFile, lstat, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { parse, stringify } from 'yaml';

const [version, input, output] = process.argv.slice(2);
if (!version || !input || !output || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-(alpha|beta|rc)\.(0|[1-9]\d*))?$/.test(version)) {
  throw new Error('Usage: node scripts/assemble-release.mjs <version> <input-dir> <output-dir>');
}

const channel = version.includes('-') ? version.split('-')[1].split('.')[0] : 'latest';
const platforms = [
  { dir: 'mac-arm64', channel: `${channel}-mac.yml`, files: [`Crab-${version}-mac-arm64.zip`, `Crab-${version}-mac-arm64.dmg`] },
  { dir: 'mac-x64', channel: `${channel}-mac.yml`, files: [`Crab-${version}-mac-x64.zip`, `Crab-${version}-mac-x64.dmg`] },
  { dir: 'windows-x64', channel: `${channel}.yml`, files: [`Crab-${version}-win-x64.exe`] },
  { dir: 'linux-x64', channel: `${channel}-linux.yml`, files: [`Crab-${version}-linux-x86_64.AppImage`, `Crab-${version}-linux-amd64.deb`] },
];

async function digest(file) {
  const stat = await lstat(file);
  if (!stat.isFile() || stat.size === 0) throw new Error(`Expected a nonempty regular file: ${file}`);
  const sha512 = createHash('sha512');
  const sha256 = createHash('sha256');
  for await (const chunk of createReadStream(file)) {
    sha512.update(chunk);
    sha256.update(chunk);
  }
  return { size: stat.size, sha512: sha512.digest('base64'), sha256: sha256.digest('hex') };
}

const assets = new Map();
const channels = [];
for (const platform of platforms) {
  const dir = path.join(input, platform.dir);
  const metadata = parse(await readFile(path.join(dir, platform.channel), 'utf8'));
  if (!metadata || metadata.version !== version || !Array.isArray(metadata.files) || metadata.files.length === 0 || metadata.packages) {
    throw new Error(`Invalid ${platform.dir} update metadata or version`);
  }
  const references = new Map();
  for (const file of metadata.files) {
    if (!file || !platform.files.includes(file.url) || references.has(file.url)) {
      throw new Error(`Unexpected or duplicate ${platform.dir} update file: ${file?.url}`);
    }
    references.set(file.url, file);
  }
  if (!references.has(platform.files[0])) throw new Error(`Missing update installer for ${platform.dir}`);
  if (platform.dir === 'linux-x64' && !references.has(platform.files[1])) {
    throw new Error('Missing DEB update installer for linux-x64');
  }
  if (!references.has(metadata.path) || references.get(metadata.path).sha512 !== metadata.sha512) {
    throw new Error(`Invalid ${platform.dir} update path or checksum`);
  }
  for (const name of platform.files) {
    const file = path.join(dir, name);
    const hashes = await digest(file);
    const reference = references.get(name);
    if (reference && (reference.sha512 !== hashes.sha512 || (reference.size != null && reference.size !== hashes.size))) {
      throw new Error(`Update metadata does not match ${name}`);
    }
    assets.set(name, { file, ...hashes });
    const blockmap = `${file}.blockmap`;
    try {
      const hashes = await digest(blockmap);
      assets.set(`${name}.blockmap`, { file: blockmap, ...hashes });
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
    }
  }
  channels.push(metadata);
}

// Both Mac architectures use the same update channel, so retain their common
// release policy while combining the architecture-specific installer records.
const [arm, x64, windows, linux] = channels;
const policy = ({ files, path, sha512, sha2, releaseDate, ...rest }) => rest;
if (JSON.stringify(policy(arm)) !== JSON.stringify(policy(x64))) {
  throw new Error('macOS update metadata has conflicting release policy');
}
const mac = { ...arm, files: [...arm.files, ...x64.files] };
for (const [name, content] of [
  [`${channel}-mac.yml`, stringify(mac)],
  [`${channel}.yml`, stringify(windows)],
  [`${channel}-linux.yml`, stringify(linux)],
]) {
  const data = Buffer.from(content);
  assets.set(name, { data, sha256: createHash('sha256').update(data).digest('hex') });
}

await mkdir(output, { recursive: true });
for (const [name, asset] of assets) {
  const dest = path.join(output, name);
  if (asset.file) await copyFile(asset.file, dest);
  else await writeFile(dest, asset.data);
}
const checksums = [...assets].sort(([a], [b]) => a.localeCompare(b))
  .map(([name, asset]) => `${asset.sha256}  ${name}\n`).join('');
await writeFile(path.join(output, 'SHA256SUMS-desktop.txt'), checksums);
