import { rebuild } from '@electron/rebuild';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const desktop = fileURLToPath(new URL('../', import.meta.url));
const { version: electronVersion } = require('electron/package.json');

await rebuild({ buildPath: desktop, electronVersion });

// macOS prebuilds have a non-executable spawn-helper. Source rebuilding keeps
// the executable helper and its native module matched in development and packages.
if (process.platform === 'darwin') {
  await rebuild({ buildPath: desktop, electronVersion, force: true, onlyModules: ['node-pty'], buildFromSource: true });
}
