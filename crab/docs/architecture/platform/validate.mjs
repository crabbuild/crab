import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// Validate design inputs only; this does not exercise a platform implementation.
const root = path.dirname(fileURLToPath(import.meta.url));
const contracts = path.join(root, 'contracts');
const runtime = readFileSync(path.join(contracts, 'runtime.sql'), 'utf8');
let assertions = 0;

function sqlite(schema, operation, expected = '') {
  const result = spawnSync('sqlite3', ['-batch', '-bail', ':memory:'], {
    input: runtime + '\n' + schema + '\n' + operation,
    encoding: 'utf8',
  });
  if (result.error) throw result.error;
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), expected);
  assertions++;
}

function rejects(schema, operation, reason) {
  const result = spawnSync('sqlite3', ['-batch', '-bail', ':memory:'], {
    input: runtime + '\n' + schema + '\n' + operation,
    encoding: 'utf8',
  });
  if (result.error) throw result.error;
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, reason);
  assertions++;
}

const kv = readFileSync(path.join(contracts, 'kv.sql'), 'utf8');
const queue = readFileSync(path.join(contracts, 'queue.sql'), 'utf8');
const workflow = readFileSync(path.join(contracts, 'workflow.sql'), 'utf8');
for (const schema of ['', kv, queue, workflow]) sqlite(schema, 'PRAGMA integrity_check;', 'ok');

rejects('', `INSERT INTO sys_meta VALUES(1, zeroblob(31), zeroblob(16), 0, 0, 1);`, /CHECK constraint/);
rejects(kv, `INSERT INTO kv_entries VALUES(X'', X'01', zeroblob(27), X'', NULL);`, /CHECK constraint/);
rejects(kv, `INSERT INTO kv_entries VALUES(X'', X'01', zeroblob(28), zeroblob(65537), NULL);`, /CHECK constraint/);

const leased = `INSERT INTO queue_messages VALUES(zeroblob(16), X'01', 1, 1, 0, 1000, zeroblob(16), 100, NULL);`;
rejects(queue, `INSERT INTO queue_messages VALUES(zeroblob(16), X'01', 1, 1, 0, 1000, NULL, 100, NULL);`, /CHECK constraint/);
rejects(queue, leased + `UPDATE queue_messages SET state=2;`, /CHECK constraint/);
sqlite(queue, leased + `
UPDATE queue_messages SET state=2, token=NULL, lease_until_ms=NULL
WHERE message_id=zeroblob(16) AND state=1 AND token=X'01' AND lease_until_ms>50;
SELECT changes();
UPDATE queue_messages SET state=2, token=NULL, lease_until_ms=NULL
WHERE message_id=zeroblob(16) AND state=1 AND token=zeroblob(16) AND lease_until_ms>50;
SELECT changes();`, '0\n1');

const run = `INSERT INTO workflow_runs VALUES(X'01', zeroblob(16), zeroblob(32), 0, X'', 0, NULL, NULL);`;
rejects(workflow, run + `
INSERT INTO workflow_events VALUES(zeroblob(16), 1, zeroblob(32), zeroblob(32), X'');
INSERT INTO workflow_events VALUES(zeroblob(16), 2, zeroblob(32), zeroblob(32), X'');`, /UNIQUE constraint/);
rejects(workflow, `INSERT INTO workflow_events VALUES(zeroblob(16), 1, zeroblob(32), zeroblob(32), X'');`, /FOREIGN KEY constraint/);
const activity = `INSERT INTO workflow_activities VALUES(zeroblob(16), zeroblob(16), 'build', X'', 0, 0, 0, 1000, NULL, NULL, NULL, NULL, NULL);`;
rejects(workflow, run + activity + `UPDATE workflow_activities SET completion_token=zeroblob(16);`, /CHECK constraint/);
rejects(workflow, run + activity + `UPDATE workflow_activities SET state=1, lease_until_ms=100;`, /CHECK constraint/);
rejects('', `INSERT INTO sys_effects VALUES(zeroblob(32), zeroblob(32), X'', 1, 1, 0, 1000, NULL, 100, 1, NULL);`, /CHECK constraint/);

const scratch = mkdtempSync(path.join(tmpdir(), 'crab-platform-contracts-'));
try {
  const compile = spawnSync('protoc', [
    `--proto_path=${contracts}`,
    `--descriptor_set_out=${path.join(scratch, 'platform.pb')}`,
    'platform.proto',
  ], { encoding: 'utf8' });
  if (compile.error) throw compile.error;
  assert.equal(compile.status, 0, compile.stderr);
  assertions++;
  const encoded = spawnSync('protoc', [
    `--proto_path=${contracts}`, '--encode=crab.platform.v1.MutationRequest', 'platform.proto',
  ], { input: `target { binding: "sql" partition: "p" }
identity { request_id: "1234567890123456" incarnation: "abcdefghijklmnop" issued_at_ms: 1 expires_at_ms: 2 }
sql_batch { statements { sql: "SELECT ?" parameters { integer: 9223372036854775807 } } }` });
  if (encoded.error) throw encoded.error;
  assert.equal(encoded.status, 0, encoded.stderr.toString());
  const decoded = spawnSync('protoc', [
    `--proto_path=${contracts}`, '--decode=crab.platform.v1.MutationRequest', 'platform.proto',
  ], { input: encoded.stdout, encoding: 'utf8' });
  assert.equal(decoded.status, 0, decoded.stderr);
  assert.match(decoded.stdout, /integer: 9223372036854775807/);
  assertions++;
} finally {
  // Only the unique directory created by this validation run is removed.
  rmSync(scratch, { recursive: true });
}

let links = 0;
for (const name of readdirSync(root).filter(name => name.endsWith('.md'))) {
  const text = readFileSync(path.join(root, name), 'utf8');
  assert.equal((text.match(/^```/gm) || []).length % 2, 0, `${name}: unbalanced fences`);
  assert(!/[\t ]+$/m.test(text), `${name}: trailing whitespace`);
  for (const match of text.matchAll(/\[[^\]]*\]\(([^)]+)\)/g)) {
    if (/^https?:/.test(match[1])) continue;
    const [file, anchor] = match[1].split('#');
    const target = path.resolve(root, file || name);
    assert(existsSync(target), `${name}: missing ${match[1]}`);
    if (anchor && target.endsWith('.md')) {
      const headings = [...readFileSync(target, 'utf8').matchAll(/^#+\s+(.+)$/gm)]
        .map(m => m[1].toLowerCase().replace(/[^\p{L}\p{N}_\-\s]/gu, '').replace(/ /g, '-'));
      assert(headings.includes(anchor), `${name}: missing anchor ${match[1]}`);
    }
    links++;
  }
}
console.log(`Passed ${assertions} schema/protocol assertions and ${links} local links; Markdown fences/whitespace valid.`);
