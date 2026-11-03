// Standalone runner for `src/lib/dataset-kind.test.ts`.
//
// Same pattern as `discard-threshold-check.mjs`: bundle the TS with
// esbuild into an ESM module, import it, let the test script run
// its assertions. Exits non-zero on failure.
//
// Run: `node scripts/dataset-kind-check.mjs`

import { execSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const tmp = join(tmpdir(), `crab-dataset-kind-${Date.now()}`);
mkdirSync(tmp, { recursive: true });

const out = join(tmp, "dataset-kind.test.mjs");
execSync(
  `npx --yes esbuild src/lib/dataset-kind.test.ts --bundle --format=esm --platform=node --outfile=${out}`,
  { stdio: "inherit" },
);

await import(`file://${out}`);
