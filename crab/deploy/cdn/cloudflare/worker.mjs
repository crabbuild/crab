const MAX_TRANSFER_BYTES = 8 * 1024 * 1024;
const PREFIX = /^[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)*$/;
const TOKEN = /^[A-Za-z0-9_-]{43}$/;
const CACHE_NAMESPACE = /^[A-Za-z0-9_-]{1,64}$/;
const DIGEST = /^[a-f0-9]{64}$/;
const encoder = new TextEncoder();

// The bucket is dedicated to this repository. Bucket-global Xet keys cannot
// serve as an authorization boundary in a bucket shared by multiple repos.
function immutableKey(key, prefix) {
  const fanout = (parts) =>
    parts.length === 2 && DIGEST.test(parts[1]) && parts[0] === parts[1].slice(0, 2);
  if (key.startsWith(".crab/")) {
    const [kind, ...parts] = key.slice(6).split("/");
    return ["xorbs", "shards", "capsule-chunk-pages"].includes(kind) && fanout(parts);
  }
  if (!key.startsWith(prefix + "/")) return false;
  const [namespace, kind, ...parts] = key.slice(prefix.length + 1).split("/");
  if (namespace === "v2") {
    if (kind === "history" && parts[0] === "source-roots") return fanout(parts.slice(1));
    return ["history", "capsules", "checkpoints", "catalogs", "pack-layers"].includes(kind)
      && fanout(parts);
  }
  return namespace === "lfs" && kind === "objects" && parts.length === 3
    && fanout([parts[0], parts[2]]) && parts[1] === parts[2].slice(2, 4);
}

function rejection(status) {
  return new Response(null, { status, headers: { "Cache-Control": "private, no-store" } });
}

async function authorized(request, secret) {
  const header = request.headers.get("Authorization");
  if (!header?.startsWith("Bearer ") || !secret || !TOKEN.test(secret)) return false;
  const token = header.slice(7);
  if (!TOKEN.test(token)) return false;
  const [actual, expected] = await Promise.all(
    [token, secret].map((value) => crypto.subtle.digest("SHA-256", encoder.encode(value))),
  );
  return crypto.subtle.timingSafeEqual(actual, expected);
}

function objectRange(value) {
  if (value === null) return null;
  const match = /^bytes=(0|[1-9]\d*)-(0|[1-9]\d*)$/.exec(value);
  if (!match) return undefined;
  const start = Number(match[1]);
  const end = Number(match[2]) + 1;
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)
      || end <= start || end - start > MAX_TRANSFER_BYTES) return undefined;
  return { start, end };
}

function deliver(response, range, cacheStatus) {
  const total = Number(response.headers.get("X-Crab-Object-Size"));
  const length = range ? range.end - range.start : total;
  const headers = new Headers({
    "Cache-Control": "private, no-store",
    "Content-Type": "application/octet-stream",
    "Content-Length": String(length),
    "X-Crab-CDN-Cache": cacheStatus,
  });
  if (range) headers.set("Content-Range", `bytes ${range.start}-${range.end - 1}/${total}`);
  return new Response(response.body, { status: range ? 206 : 200, headers });
}

export default {
  async fetch(request, env, ctx) {
    if (request.method !== "GET") return rejection(405);
    const url = new URL(request.url);
    const key = url.pathname.slice(1);
    const prefix = env.CRAB_REPO_PREFIX;
    if (!prefix || !PREFIX.test(prefix)
        || !CACHE_NAMESPACE.test(env.CRAB_CACHE_NAMESPACE ?? "")
        || prefix.split("/").some((part) => [".", "..", ".crab"].includes(part))
        || !immutableKey(key, prefix)
        || url.search || url.hostname !== env.CRAB_VIEWER_HOST) {
      return rejection(403);
    }
    if (!await authorized(request, env.CRAB_VIEWER_TOKEN)) return rejection(403);
    const range = objectRange(request.headers.get("Range"));
    if (range === undefined) return rejection(416);

    // Cache exact ranges as internal 200s. A cold sparse read goes to R2 for
    // only the requested bytes. The viewer never receives the internal key.
    const window = range ? `${range.start}-${range.end - 1}` : "whole";
    const cacheKey = new Request(`${url.origin}/.crab-edge/v1/${env.CRAB_CACHE_NAMESPACE}/${window}/${key}`);
    const cached = await caches.default.match(cacheKey);
    if (cached) return deliver(cached, range, "HIT");

    const object = range
      ? await env.CRAB_BUCKET.get(key, { range: { offset: range.start, length: range.end - range.start } })
      : await env.CRAB_BUCKET.get(key);
    if (!object) return rejection(404);
    const total = object.size;
    if (!Number.isSafeInteger(total) || total < 0 || (range ? range.end > total : total > MAX_TRANSFER_BYTES)) {
      await object.body.cancel();
      return rejection(416);
    }
    const length = range ? range.end - range.start : total;
    if (range && (object.range?.offset !== range.start || object.range?.length !== length)) {
      await object.body.cancel();
      return rejection(502);
    }
    const stored = new Response(object.body, {
      headers: {
        "Cache-Control": "public, max-age=3600",
        "Content-Type": "application/octet-stream",
        "Content-Length": String(length),
        "X-Crab-Object-Size": String(total),
      },
    });
    ctx.waitUntil(caches.default.put(cacheKey, stored.clone()).catch(() => {}));
    return deliver(stored, range, "MISS");
  },
};
