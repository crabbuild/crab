// Standalone runner for `src/lib/pointer-detection.test.ts`.
//
// Same pattern as `file-state-check.mjs` / `parse-patch-check.mjs`:
// bundle the TS with esbuild into an ESM module, import it, let
// the test script run its assertions. Exits non-zero on failure.
//
// Run: `node scripts/pointer-detection-check.mjs`

import { execSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const tmp = join(tmpdir(), `crab-pointer-detection-${Date.now()}`);
mkdirSync(tmp, { recursive: true });

const out = join(tmp, "pointer-detection.test.mjs");
execSync(
  `npx --yes esbuild src/lib/pointer-detection.test.ts --bundle --format=esm --platform=node --outfile=${out}`,
  { stdio: "inherit" },
);

await import(`file://${out}`);
