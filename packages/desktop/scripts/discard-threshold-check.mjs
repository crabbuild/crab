// Standalone runner for `src/lib/discard-threshold.test.ts`.
//
// Same pattern as `parse-patch-check.mjs` / `file-state-check.mjs`:
// bundle the TS with esbuild into an ESM module, import it, let
// the test script run its assertions. Exits non-zero on failure.
//
// Run: `node scripts/discard-threshold-check.mjs`

import { execSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const tmp = join(tmpdir(), `crab-discard-threshold-${Date.now()}`);
mkdirSync(tmp, { recursive: true });

const out = join(tmp, "discard-threshold.test.mjs");
execSync(
  `npx --yes esbuild src/lib/discard-threshold.test.ts --bundle --format=esm --platform=node --outfile=${out}`,
  { stdio: "inherit" },
);

await import(`file://${out}`);
