#!/usr/bin/env node
// Rename tsc's emitted .js files (and their .js.map companions) to .cjs
// so Node always loads them as CommonJS, regardless of the enclosing
// package.json's `type` field.
//
// Why this exists: Electron's main process expects CJS, and shipping
// `.cjs` is the most robust signal to Node. Relying on "type": "commonjs"
// in package.json works only when electron-builder doesn't rewrite it,
// which isn't something we want to depend on across builder versions.

import { readdir, readFile, rename, stat, writeFile, mkdir, copyFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const OUT_DIR = fileURLToPath(new URL("../dist-electron/", import.meta.url));
const MANIFESTS_SRC = resolve(fileURLToPath(new URL("../electron/manifests/", import.meta.url)));
const MANIFESTS_DEST = resolve(OUT_DIR, "manifests");

async function walk(dir) {
  const out = [];
  for (const name of await readdir(dir)) {
    const full = join(dir, name);
    const s = await stat(full);
    if (s.isDirectory()) out.push(...(await walk(full)));
    else out.push(full);
  }
  return out;
}

async function renameJsToCjs() {
  const entries = await walk(OUT_DIR);
  const jsFiles = entries.filter((p) => p.endsWith(".js"));
  const mapFiles = entries.filter((p) => p.endsWith(".js.map"));

  // Rename .js -> .cjs first so subsequent map edits can find them.
  for (const js of jsFiles) {
    const cjs = js.slice(0, -".js".length) + ".cjs";
    await rename(js, cjs);
    let content = await readFile(cjs, "utf8");
    // Rewrite //# sourceMappingURL=foo.js.map -> foo.cjs.map
    content = content.replace(
      /\/\/# sourceMappingURL=(.+)\.js\.map$/m,
      (_, base) => `//# sourceMappingURL=${base}.cjs.map`,
    );
    // Rewrite relative require()s so they resolve to the .cjs siblings.
    // Node < 20 does not auto-resolve `./foo` to `foo.cjs` — it only
    // searches for `.js` / `.json` / `.node`. Adding the extension
    // explicitly makes the bundle portable across the Electron-bundled
    // Node versions we care about.
    content = content.replace(
      /require\((['"])(\.\.?\/[^'"]+?)\1\)/g,
      (match, quote, specifier) => {
        // Skip specifiers that already carry an extension.
        if (/\.(c?js|json|node|mjs)$/.test(specifier)) return match;
        return `require(${quote}${specifier}.cjs${quote})`;
      },
    );
    // Node16 preserves native import(), including ESM-only dependencies.
    content = content.replace(/import\((['"])(\.\.?\/[^'"]+)\.js\1\)/g,
      (_, quote, specifier) => `import(${quote}${specifier}.cjs${quote})`);
    await writeFile(cjs, content);
  }

  for (const map of mapFiles) {
    const cjsMap = map.slice(0, -".js.map".length) + ".cjs.map";
    await rename(map, cjsMap);
    // Rewrite the `file` field inside the source map so debuggers
    // resolve the emitted file by its new name.
    const content = await readFile(cjsMap, "utf8");
    try {
      const parsed = JSON.parse(content);
      if (typeof parsed.file === "string" && parsed.file.endsWith(".js")) {
        parsed.file = parsed.file.slice(0, -".js".length) + ".cjs";
        await writeFile(cjsMap, JSON.stringify(parsed));
      }
    } catch {
      // Non-JSON source maps are vanishingly rare but not worth failing on.
    }
  }
}

await renameJsToCjs();
await copyManifests();
await verifyDaemonArtifact();

/**
 * Copies the agent-manifest JSON files from `electron/manifests/` to
 * `dist-electron/manifests/`. `tsc` only emits `.ts` → `.js` output;
 * without this copy, the packaged Electron app ships no built-in
 * manifests and the registry falls back to built-ins only.
 */
async function copyManifests() {
  try {
    const entries = await readdir(MANIFESTS_SRC);
    const jsonFiles = entries.filter((f) => f.endsWith(".json"));
    if (jsonFiles.length === 0) return;
    await mkdir(MANIFESTS_DEST, { recursive: true });
    for (const file of jsonFiles) {
      await copyFile(join(MANIFESTS_SRC, file), join(MANIFESTS_DEST, file));
    }
  } catch (err) {
    // Log but don't fail the build — the registry tolerates a missing
    // manifest directory.
    console.warn(`[rename-to-cjs] Failed to copy manifests: ${err instanceof Error ? err.message : String(err)}`);
  }
}

/**
 * Verifies that the daemon entry point was produced by the build.
 * Fails with a non-zero exit if the file is missing so CI catches
 * packaging regressions before the binary ships.
 */
async function verifyDaemonArtifact() {
  const daemonEntry = resolve(OUT_DIR, "daemon", "daemon-main.cjs");
  try {
    await stat(daemonEntry);
  } catch {
    console.error(
      `[rename-to-cjs] Build error: expected daemon entry point not found.\n` +
        `  Searched: ${daemonEntry}\n` +
        `  Ensure that 'electron/daemon/daemon-main.ts' is included in tsconfig.electron.json ` +
        `and that the TypeScript compilation succeeded.`,
    );
    process.exit(1);
  }
}
