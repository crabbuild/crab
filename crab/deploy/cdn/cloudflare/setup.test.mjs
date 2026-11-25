import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const script = fileURLToPath(new URL("./setup.mjs", import.meta.url));
const account = "0123456789abcdef0123456789abcdef";
const endpoint = `https://${account}.eu.r2.cloudflarestorage.com`;

test("setup pins matching Worker and Crab targets and preserves an existing token", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "crab-r2-setup-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const output = join(parent, "deployment");
  const args = [
    script, "--r2-endpoint", endpoint + "/", "--bucket", "fixture-bucket",
    "--repository", "org/repo", "--hostname", "cdn.example.com", "--output", output,
  ];
  const setup = spawnSync(process.execPath, args, { encoding: "utf8" });
  assert.equal(setup.status, 0, setup.stderr);
  const config = JSON.parse(await readFile(join(output, "wrangler.json"), "utf8"));
  const client = await readFile(join(output, "crab-cdn.toml"), "utf8");
  const viewer = join(output, "viewer-token");
  const token = (await readFile(viewer, "utf8")).trimEnd();
  assert.equal(config.account_id, account);
  assert.equal(config.main, "worker.mjs");
  assert.equal(config.workers_dev, false);
  assert.equal(config.preview_urls, false);
  assert.equal(config.routes, undefined);
  assert.deepEqual(config.r2_buckets, [
    { binding: "CRAB_BUCKET", bucket_name: "fixture-bucket", jurisdiction: "eu" },
  ]);
  assert.deepEqual(config.vars, {
    CRAB_REPO_PREFIX: "org/repo",
    CRAB_VIEWER_HOST: "cdn.example.com",
    CRAB_CACHE_NAMESPACE: config.name,
  });
  assert.deepEqual(config.secrets.required, ["CRAB_VIEWER_TOKEN"]);
  assert.equal(client, [
    "[[cdn]]",
    `origin = "${endpoint}/fixture-bucket"`,
    'repo_prefix = "org/repo"',
    'base_url = "https://cdn.example.com"',
    'access = "private"',
    `viewer_token_file = ${JSON.stringify(viewer)}`,
    "",
  ].join("\n"));
  assert.deepEqual(await readFile(join(output, "worker.mjs")), await readFile(new URL("./worker.mjs", import.meta.url)));
  assert.match(token, /^[A-Za-z0-9_-]{43}$/);
  assert.ok(!JSON.stringify(config).includes(token) && !client.includes(token) && !setup.stdout.includes(token));
  if (process.platform !== "win32") {
    assert.equal((await stat(viewer)).mode & 0o077, 0);
  }
  const repeated = spawnSync(process.execPath, args, { encoding: "utf8" });
  assert.equal(repeated.status, 1);
  assert.match(repeated.stderr, /existing deployments are preserved/);
  assert.equal((await readFile(viewer, "utf8")).trimEnd(), token);
});

test("setup rejects a non-R2 origin and Cloudflare development hostnames", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "crab-r2-setup-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const output = join(parent, "deployment");
  for (const [origin, hostname] of [
    [endpoint + "/fixture-bucket", "cdn.example.com"],
    [endpoint, "fixture.workers.dev"],
    [endpoint, "fixture.r2.dev"],
  ]) {
    const result = spawnSync(process.execPath, [
      script, "--r2-endpoint", origin, "--bucket", "fixture-bucket",
      "--repository", "org/repo", "--hostname", hostname, "--output", output,
    ]);
    assert.equal(result.status, 1);
    await assert.rejects(stat(output), { code: "ENOENT" });
  }
});
