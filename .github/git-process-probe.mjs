import { execFileSync, spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const root = mkdtempSync(path.join(tmpdir(), 'crab-git-process-'));
const binary = path.resolve(process.argv[2]);
const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' });
try {
  git('init', '--initial-branch=main');
  const core = git('--exec-path').trim();
  const realGit = path.resolve(core, '..', '..', 'bin', 'git.exe');
  const cases = [['default', 'git'], ['no-console', 'git'], ['null-stdin', 'git']];
  if (existsSync(realGit)) cases.push(['direct-git', realGit]);
  for (const [mode, executable] of cases) {
    console.log(`Probe: ${mode}`);
    const child = spawn(binary, [mode, root, executable], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    child.stdin.write('go\n');
    child.stderr.resume();
    readline.createInterface({ input: child.stdout }).on('line', line => {
      console.log(line);
      if (line.startsWith('finished config --get')) child.stdin.end();
    });
    const status = await new Promise(resolve => {
      const timer = setTimeout(() => {
        console.log(`${mode}: timed out`);
        try { execFileSync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore', timeout: 5000 }); }
        catch { child.kill(); }
      }, 8000);
      child.once('exit', code => { clearTimeout(timer); resolve(code); });
      child.once('error', error => { clearTimeout(timer); resolve(error.code); });
    });
    console.log(`${mode} result: ${status}`);
  }
} finally {
  rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
