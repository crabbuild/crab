// Standalone runner for `src/lib/parse-patch.test.ts`.
//
// Mirrors `commit-graph-check.mjs`: bundle the TS source with
// esbuild into an ESM module, import it, let the test script run
// its assertions. Exits non-zero on failure.
//
// Run: `node scripts/parse-patch-check.mjs`

import { execSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const tmp = join(tmpdir(), `crab-parse-patch-${Date.now()}`);
mkdirSync(tmp, { recursive: true });

const out = join(tmp, "parse-patch.test.mjs");
execSync(
  `npx --yes esbuild src/lib/parse-patch.test.ts --bundle --format=esm --platform=node --outfile=${out}`,
  { stdio: "inherit" },
);

await import(`file://${out}`);
