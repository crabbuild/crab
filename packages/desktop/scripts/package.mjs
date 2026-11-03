import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const desktop = fileURLToPath(new URL('../', import.meta.url));
const { version } = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
const channel = version.includes('-') ? version.split('-')[1].split('.')[0] : 'latest';
// GitHub publishing does not infer channels in electron-builder 25. Keep local
// and hosted packages identical; the release workflow owns publication.
const child = spawn(process.execPath, [
  require.resolve('electron-builder/cli.js'), ...process.argv.slice(2),
  '--publish', 'never', `--config.publish.channel=${channel}`,
], { cwd: desktop, stdio: 'inherit' });
child.once('error', error => { throw error; });
child.once('exit', code => { process.exitCode = code ?? 1; });
