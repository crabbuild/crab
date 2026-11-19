#!/usr/bin/env node
// Real Chromium HTTP-cache proof; routing mocks would disable the cache.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, stat, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(new URL("../../../packages/ui/package.json", import.meta.url));
const { chromium } = require("@playwright/test");
if (process.argv.includes("--help")) {
  console.log("verify_v2_browser_revalidation.mjs --origin URL --path PREVIEW_PATH --headers-file PRIVATE_JSON --output REPORT_JSON");
  process.exit(0);
}
const argumentsByName = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  assert.ok(process.argv[index].startsWith("--") && process.argv[index + 1]);
  argumentsByName.set(process.argv[index].slice(2), process.argv[index + 1]);
}
assert.deepEqual([...argumentsByName.keys()].sort(), ["headers-file", "origin", "output", "path"]);
const origin = new URL(argumentsByName.get("origin"));
assert.ok(["127.0.0.1", "localhost", "[::1]"].includes(origin.hostname));
const path = argumentsByName.get("path");
assert.ok(path.startsWith("/api/repos/") && path.includes("/file-preview?"));
const headersFile = argumentsByName.get("headers-file");
const details = await stat(headersFile);
assert.equal(details.mode & 0o077, 0);
assert.equal(details.uid, process.getuid());
assert.ok(details.isFile() && details.size <= 16384);
const headers = JSON.parse(await readFile(headersFile, "utf8"));
const cookie = headers.Cookie ?? headers.cookie;
assert.equal(typeof cookie, "string");
const hash = (value) => createHash("sha256").update(value).digest("hex");
const browser = await chromium.launch({ headless: true });
const report = { schema: 1, status: "running", checks: [] };
try {
  const context = await browser.newContext();
  await context.addCookies(cookie.split(";").map((entry) => {
    const separator = entry.indexOf("=");
    assert.ok(separator > 0);
    return { name: entry.slice(0, separator).trim(), value: entry.slice(separator + 1).trim(), url: origin.origin };
  }));
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  const requests = new Set();
  const statuses = [];
  await cdp.send("Network.enable");
  const url = new URL(path, origin).href;
  cdp.on("Network.requestWillBeSent", (event) => {
    if (event.request.url === url) requests.add(event.requestId);
  });
  cdp.on("Network.responseReceivedExtraInfo", (event) => {
    if (requests.has(event.requestId)) statuses.push(event.statusCode);
  });
  await page.goto(new URL("/livez", origin).href);
  const read = () => page.evaluate(async (path) => {
    const response = await fetch(path, { headers: { Accept: "application/json" } });
    return { status: response.status, text: await response.text(), etag: response.headers.get("etag"), cache: response.headers.get("cache-control") };
  }, path);
  const first = await read();
  assert.equal(first.status, 200);
  assert.equal(first.cache, "private, no-cache");
  assert.ok(first.etag?.startsWith('"'));
  assert.ok(JSON.parse(first.text).preview);
  const second = await read();
  assert.equal(second.status, 200);
  assert.equal(hash(first.text), hash(second.text));
  assert.equal(second.etag, first.etag);
  assert.ok(statuses.includes(304), "Chromium must revalidate through a real wire 304");
  report.checks.push("native HTTP cache revalidated", "fetch returned its retained JSON body");
  await context.clearCookies();
  const denied = await read();
  assert.equal(denied.status, 401);
  assert.equal(denied.cache, "no-store");
  assert.equal(JSON.parse(denied.text).preview, undefined);
  report.checks.push("removed authentication prevented cached success");
  report.browser = browser.version();
  report.wire_statuses = statuses;
  report.body_sha256 = hash(first.text);
  report.tool_sha256 = hash(await readFile(fileURLToPath(import.meta.url)));
  report.status = "passed";
} catch (error) {
  report.status = "failed";
  report.error_kind = error.constructor.name;
  throw error;
} finally {
  await browser.close();
  await writeFile(argumentsByName.get("output"), JSON.stringify(report, null, 2) + "\n", { flag: "wx", mode: 0o600 });
}
console.log("Real browser revalidation checks passed");
