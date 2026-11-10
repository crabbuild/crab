import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const desktop = fileURLToPath(new URL('../', import.meta.url));
const cli = require.resolve('@electron/rebuild/lib/cli.js');
const rebuilds = [[]];

// macOS prebuilds have a non-executable spawn-helper. Source rebuilding keeps
// the executable helper and its native module matched in development and packages.
if (process.platform === 'darwin') {
  rebuilds.push(['--force', '--only', 'node-pty', '--build-from-source']);
}

for (const options of rebuilds) {
  const result = spawnSync(process.execPath, [cli, ...options], { cwd: desktop, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
