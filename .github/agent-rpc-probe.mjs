import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';

const binary = path.resolve(process.argv[2]);
const directory = mkdtempSync(path.join(tmpdir(), 'crab-agent-probe-'));
const content = 'Crab packaged agent probe\n';
const git = (...args) => execFileSync('git', args, { cwd: directory, stdio: 'pipe' });
const trace = path.join(directory, 'git-trace.jsonl');
let child;
let exited;
try {
  git('init', '--initial-branch=main');
  writeFileSync(path.join(directory, 'probe.txt'), content);
  git('add', 'probe.txt');
  git('-c', 'user.name=Qualification', '-c', 'user.email=qualify@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-m', 'Probe');
  child = spawn(binary, [], {
    cwd: directory, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, CRAB_DESKTOP_LOG: 'debug', GIT_TRACE2_EVENT: trace,
      CRAB_BIN: path.join(path.dirname(binary), process.platform === 'win32' ? 'crab.exe' : 'crab') },
  });
  // Restrict diagnostics to method, outcome, and timing. No payloads or native logs.
  createInterface({ input: child.stderr }).on('line', line => {
    for (const marker of ['opened local repo', 'failed to dispatch request', 'Indexer started']) {
      if (line.includes(marker)) console.log(`native agent: ${marker}`);
    }
  });
  const frames = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  let nextId = 0;
  exited = new Promise(resolve => {
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
} catch (error) {
  console.error(error.message);
  if (existsSync(trace)) {
    for (const line of readFileSync(trace, 'utf8').trim().split('\n')) {
      try {
        const { event, sid, code, name } = JSON.parse(line);
        if (['start', 'cmd_name', 'exit', 'atexit', 'child_start', 'child_exit'].includes(event)) {
          console.log('Git Trace2:', JSON.stringify({ event, sid, code, name }));
        }
      } catch { /* A still-running Git process may have a partial final event. */ }
    }
  } else console.log('Git did not write a Trace2 event');
  if (process.platform === 'win32' && child?.pid) {
    const script = `Get-CimInstance Win32_Process -Filter 'ParentProcessId = ${child.pid}' | Select-Object ProcessId,Name | ConvertTo-Json -Compress`;
    try { console.log('Remaining sidecar children:', execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8', timeout: 5000 }).trim()); }
    catch { console.log('Unable to query sidecar children'); }
  }
  process.exitCode = 1;
} finally {
  if (child?.pid && child.exitCode === null) {
    if (process.platform === 'win32') {
      try { execFileSync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore', timeout: 5000 }); }
      catch { child.kill(); }
    } else child.kill();
    await exited;
  }
  rmSync(directory, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
