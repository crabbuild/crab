import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// Validate design inputs only; this does not exercise an embedded runtime implementation.
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
const blob = readFileSync(path.join(contracts, 'blob.sql'), 'utf8');
const cron = readFileSync(path.join(contracts, 'cron.sql'), 'utf8');
for (const schema of ['', kv, queue, workflow, blob, cron]) sqlite(schema, 'PRAGMA integrity_check;', 'ok');

rejects('', `INSERT INTO sys_meta VALUES(1, zeroblob(31), zeroblob(16), 0, 0, 1);`, /CHECK constraint/);
rejects(kv, `INSERT INTO kv_entries VALUES(X'', X'01', zeroblob(27), X'', NULL);`, /CHECK constraint/);
rejects(kv, `INSERT INTO kv_entries VALUES(X'', X'01', zeroblob(28), zeroblob(4194305), NULL);`, /CHECK constraint/);

const leased = `INSERT INTO queue_messages VALUES(zeroblob(16), X'01', 1, 1, 0, 1000, zeroblob(16), 100, NULL, NULL);`;
rejects(queue, `INSERT INTO queue_messages VALUES(zeroblob(16), X'01', 1, 1, 0, 1000, NULL, 100, NULL, NULL);`, /CHECK constraint/);
rejects(queue, leased + `UPDATE queue_messages SET state=2;`, /CHECK constraint/);
rejects(queue, `INSERT INTO queue_messages VALUES(zeroblob(16), X'01', 2, 1, 0, 1000, NULL, NULL, NULL, zeroblob(32));`, /CHECK constraint/);
rejects(queue, `INSERT INTO queue_messages VALUES(zeroblob(16), X'01', 3, 1, 0, 1000, NULL, NULL, NULL, zeroblob(31));`, /CHECK constraint/);
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
sqlite(workflow, run + `UPDATE workflow_runs SET status=4; SELECT status FROM workflow_runs;`, '4');

const upload = `INSERT INTO blob_uploads VALUES(zeroblob(16), X'01', zeroblob(32), 0, NULL, NULL, X'', 0, 60000, 0, NULL, 0, 0);`;
rejects(blob, upload + `INSERT INTO blob_parts VALUES(zeroblob(16), 1, zeroblob(32), 262145, NULL);`, /CHECK constraint/);
rejects(blob, `INSERT INTO blob_parts VALUES(zeroblob(16), 1, zeroblob(32), 0, NULL);`, /FOREIGN KEY constraint/);
rejects(cron, `INSERT INTO cron_schedules VALUES(zeroblob(16), 0, X'', X'', 999, 0, 0, 1, 1, 0);`, /CHECK constraint/);

// Only message contracts are intended: product HTTP APIs remain in Crab.
const peer = readFileSync(path.join(contracts, 'peer.proto'), 'utf8');
assert(!/^\s*service\s/m.test(peer), 'private contract must not generate a public service');
assert(!/\b(?:WorkflowDecision|WorkflowAction)\b/.test(peer), 'native decisions do not cross peer RPC');
assertions += 2;

const scratch = mkdtempSync(path.join(tmpdir(), 'crab-cell-contracts-'));
try {
  const compile = spawnSync('protoc', [
    `--proto_path=${contracts}`,
    `--descriptor_set_out=${path.join(scratch, 'peer.pb')}`,
    'peer.proto',
  ], { encoding: 'utf8' });
  if (compile.error) throw compile.error;
  assert.equal(compile.status, 0, compile.stderr);
  assertions++;
  const target = 'target { tenant_id: "tenant0000000001" application_id: "app0000000000001" namespace_id: "sql0000000000001" partition: "p" }';
  const identity = 'identity { request_id: "1234567890123456" incarnation: "abcdefghijklmnop" issued_at_ms: 1 expires_at_ms: 2 }';
  // Serialization fixtures only; they do not prove authentication or signature validation.
  const fixtures = [
    {
      type: 'PeerRequest',
      input: 'version: 1 hop_count: 1 remaining_ms: 1000 authorization { origin_session: "session000000001" actions: "comment.write" } mutate {'
        + target + identity + 'cell_command { command_id: 17 codec_version: 2 input: "comment" } }',
      expected: /cell_command \{\s+command_id: 17\s+codec_version: 2\s+input: "comment"/,
    },
    {
      type: 'MigrationRequest',
      input: target
        + 'incarnation: "abcdefghijklmnop" from_code: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" from_schema: 1 '
        + 'to_code: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" to_schema: 2',
      expected: /from_schema: 1\s+to_code: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"\s+to_schema: 2/,
    },
  ];
  for (const fixture of fixtures) {
    const type = 'crab.cell.peer.v1.' + fixture.type;
    const encoded = spawnSync('protoc', [
      '--proto_path=' + contracts, '--encode=' + type, 'peer.proto',
    ], { input: fixture.input });
    if (encoded.error) throw encoded.error;
    assert.equal(encoded.status, 0, encoded.stderr.toString());
    const decoded = spawnSync('protoc', [
      '--proto_path=' + contracts, '--decode=' + type, 'peer.proto',
    ], { input: encoded.stdout, encoding: 'utf8' });
    if (decoded.error) throw decoded.error;
    assert.equal(decoded.status, 0, decoded.stderr);
    assert.match(decoded.stdout, fixture.expected);
    assertions++;
  }
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
