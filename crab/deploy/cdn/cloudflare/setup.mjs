import { createHash, randomBytes } from "node:crypto";
import { mkdir, open, readFile, rmdir, unlink } from "node:fs/promises";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";

const usage = "Usage: node setup.mjs --r2-endpoint URL --bucket NAME --repository PREFIX --hostname HOST --output DIRECTORY";

async function main() {
  const { values } = parseArgs({
    options: {
      help: { type: "boolean", short: "h" },
      "r2-endpoint": { type: "string" },
      bucket: { type: "string" },
      repository: { type: "string" },
      hostname: { type: "string" },
      output: { type: "string" },
    },
  });
  if (values.help) {
    console.log(usage);
    return;
  }

  const { "r2-endpoint": endpoint, bucket, repository, hostname, output } = values;
  if (![endpoint, bucket, repository, hostname, output].every((value) => value && value === value.trim())) {
    throw new Error(usage);
  }
  const r2 = /^https:\/\/([a-f0-9]{32})(?:\.(eu|fedramp))?\.r2\.cloudflarestorage\.com\/?$/.exec(endpoint);
  if (!r2) throw new Error("Use the canonical R2 S3 API endpoint without a bucket path");
  if (!/^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$/.test(bucket)) {
    throw new Error("Use a valid R2 bucket name");
  }
  if (repository.length > 1024 || repository.split("/").some((part) =>
    !/^[A-Za-z0-9_.-]+$/.test(part) || [".", "..", ".crab"].includes(part))) {
    throw new Error("Use the physical Crab repository prefix");
  }
  const labels = hostname.split(".");
  if (hostname.length > 253 || labels.length < 2
      || labels.some((label) => !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))
      || !/[a-z]/.test(labels.at(-1))
      || /(?:^|\.)(?:workers\.dev|r2\.dev|r2\.cloudflarestorage\.com)$/.test(hostname)) {
    throw new Error("Use a dedicated Cloudflare custom hostname");
  }

  const [, accountId, jurisdiction] = r2;
  const origin = `${endpoint.replace(/\/$/, "")}/${bucket}`;
  const identity = createHash("sha256").update(`${origin}\n${repository}\n${hostname}`).digest("hex");
  const name = `crab-r2-${identity.slice(0, 24)}`;
  const directory = resolve(output);
  const tokenFile = join(directory, "viewer-token");
  const config = {
    name,
    account_id: accountId,
    main: "worker.mjs",
    compatibility_date: "2026-05-01",
    workers_dev: false,
    preview_urls: false,
    r2_buckets: [{ binding: "CRAB_BUCKET", bucket_name: bucket, ...(jurisdiction && { jurisdiction }) }],
    vars: {
      CRAB_REPO_PREFIX: repository,
      CRAB_VIEWER_HOST: hostname,
      CRAB_CACHE_NAMESPACE: name,
    },
    secrets: { required: ["CRAB_VIEWER_TOKEN"] },
  };
  const binding = {
    origin,
    repo_prefix: repository,
    base_url: `https://${hostname}`,
    access: "private",
    viewer_token_file: tokenFile,
  };
  const toml = ["[[cdn]]", ...Object.entries(binding).map(([key, value]) =>
    `${key} = ${JSON.stringify(value)}`), ""].join("\n");
  const worker = await readFile(new URL("./worker.mjs", import.meta.url));

  await mkdir(directory, { mode: 0o700 });
  const created = [];
  try {
    for (const [filename, content] of [
      [".gitignore", "/viewer-token\n/.wrangler/\n"],
      ["worker.mjs", worker],
      ["wrangler.json", JSON.stringify(config, null, 2) + "\n"],
      ["crab-cdn.toml", toml],
      ["viewer-token", randomBytes(32).toString("base64url") + "\n"],
    ]) {
      const path = join(directory, filename);
      const file = await open(path, "wx", 0o600);
      created.push(path);
      try {
        await file.writeFile(content);
      } finally {
        await file.close();
      }
    }
  } catch (error) {
    await Promise.allSettled(created.map((path) => unlink(path)));
    await rmdir(directory).catch(() => {});
    throw error;
  }
  console.log(`Created the ${name} deployment in ${directory}`);
}

main().catch((error) => {
  console.error(error.code === "EEXIST" ? "Choose a new output directory; existing deployments are preserved" : error.message);
  process.exitCode = 1;
});
