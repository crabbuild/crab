import assert from "node:assert/strict";
import { timingSafeEqual } from "node:crypto";
import test from "node:test";

import worker from "./worker.mjs";

test("private range misses read only the requested R2 bytes and warm hits still require authorization", async () => {
  const token = "a".repeat(43);
  const host = "cdn.example.com";
  const prefix = "org/repo";
  const digest = "abcd".repeat(16);
  const key = `.crab/xorbs/ab/${digest}`;
  const payload = new TextEncoder().encode("0123456789");
  const cached = new Map();
  const writes = [];
  const reads = [];
  let matches = 0;
  crypto.subtle.timingSafeEqual = (a, b) => timingSafeEqual(Buffer.from(a), Buffer.from(b));
  globalThis.caches = {
    default: {
      match: async (request) => {
        matches++;
        return cached.get(request.url)?.clone();
      },
      put: async (request, response) => {
        cached.set(request.url, new Response(await response.arrayBuffer(), { headers: response.headers }));
      },
    },
  };
  const env = {
    CRAB_BUCKET: {
      get: async (selected, options) => {
        reads.push([selected, options]);
        const data = options?.range
          ? payload.slice(options.range.offset, options.range.offset + options.range.length)
          : payload;
        return { size: payload.length, range: options?.range, body: new Response(data).body };
      },
    },
    CRAB_REPO_PREFIX: prefix,
    CRAB_VIEWER_HOST: host,
    CRAB_CACHE_NAMESPACE: "fixture",
    CRAB_VIEWER_TOKEN: token,
  };
  const get = (path, viewer = token, range = "bytes=2-4", method = "GET") =>
    worker.fetch(
      new Request(`https://${host}/${path}`, {
        method,
        headers: { Authorization: `Bearer ${viewer}`, Range: range },
      }),
      env,
      { waitUntil: (write) => writes.push(write) },
    );
  const cold = await get(key);
  assert.equal(cold.status, 206);
  assert.equal(cold.headers.get("Content-Range"), "bytes 2-4/10");
  assert.equal(cold.headers.get("X-Crab-CDN-Cache"), "MISS");
  assert.equal(await cold.text(), "234");
  await Promise.all(writes);

  assert.equal((await get(key, "b".repeat(43))).status, 403);
  assert.equal((await get(`${prefix}/v2/root.json`)).status, 403);
  assert.equal((await get(`other/repo/v2/pack-layers/ab/${digest}`)).status, 403);
  assert.equal((await get(key, token, "bytes=2-4", "PUT")).status, 405);
  assert.equal((await get(key, token, "bytes=2-4,6-9")).status, 416);
  assert.equal(matches, 1);

  const warm = await get(key);
  assert.equal(warm.headers.get("X-Crab-CDN-Cache"), "HIT");
  assert.equal(warm.headers.get("Cache-Control"), "private, no-store");
  assert.equal(await warm.text(), "234");
  assert.deepEqual(reads, [[key, { range: { offset: 2, length: 3 } }]]);
});
