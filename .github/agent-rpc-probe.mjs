import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';

const binary = path.resolve(process.argv[2]);
const directory = mkdtempSync(path.join(tmpdir(), 'crab-agent-probe-'));
const content = 'Crab packaged agent probe\n';
const git = (...args) => execFileSync('git', args, { cwd: directory, stdio: 'pipe' });
let child;
try {
  git('init', '--initial-branch=main');
  writeFileSync(path.join(directory, 'probe.txt'), content);
  git('add', 'probe.txt');
  git('-c', 'user.name=Qualification', '-c', 'user.email=qualify@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-m', 'Probe');
  child = spawn(binary, [], {
    cwd: directory, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, CRAB_BIN: path.join(path.dirname(binary), process.platform === 'win32' ? 'crab.exe' : 'crab') },
  });
  // Restrict diagnostics to method, outcome, and timing. No payloads or native logs.
  child.stderr.resume();
  const frames = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  let nextId = 0;
  const exited = new Promise(resolve => {
    child.once('exit', (code, signal) => resolve({ code, signal }));
    child.once('error', error => resolve({ code: null, signal: error.code }));
  });
  async function call(method, params) {
    const start = performance.now();
    const id = String(++nextId);
    let timer;
    const deadline = new Promise((_, reject) => { timer = setTimeout(() => reject(Error(`${method}: timed out`)), 10000); });
    const result = (async () => {
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
      for (;;) {
        const line = await frames.next();
        if (line.done) throw Error(`${method}: stdout ended`);
        const reply = JSON.parse(line.value);
        if (reply.id !== id) continue;
        if (reply.error) throw Error(`${method}: RPC error ${reply.error.code}`);
        return reply.result;
      }
    })();
    try {
      const value = await Promise.race([result, deadline, exited.then(status => { throw Error(`${method}: agent exited ${JSON.stringify(status)}`); })]);
      console.log(`${method} completed in ${(performance.now() - start).toFixed(1)}ms`);
      return value;
    } finally { clearTimeout(timer); }
  }
  assert.equal((await call('ping', {})).pong, '2.0.0-alpha.1');
  await call('open_local', { repo_id: 'probe', path: directory });
  assert.equal((await call('read_text', { repo_id: 'probe', path: 'probe.txt' })).text, content);
  await call('close', { repo_id: 'probe' });
  child.stdin.end();
  let timer;
  try {
    const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(Error('shutdown: timed out')), 10000); });
    assert.equal((await Promise.race([exited, timeout])).code, 0);
    console.log('Agent closed after stdin EOF');
  } finally { clearTimeout(timer); }
} finally {
  child?.kill();
  rmSync(directory, { recursive: true, force: true });
}
