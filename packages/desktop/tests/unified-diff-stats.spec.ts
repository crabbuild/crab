/**
 * Tests for parseDiffStats() helper.
 *
 * Validates: Requirements CHAT3.1, CHAT4.1
 */

import { describe, it, expect } from "vitest";
import { parseDiffStats } from "../src/lib/unified-diff-stats";

describe("parseDiffStats", () => {
  it("parses a standard unified diff with additions and removals", () => {
    const patch = [
      "--- a/src/utils.ts",
      "+++ b/src/utils.ts",
      "@@ -1,5 +1,6 @@",
      " import { foo } from './foo';",
      "-import { bar } from './bar';",
      "-import { baz } from './baz';",
      "+import { bar, baz } from './bar';",
      "+import { qux } from './qux';",
      "+import { quux } from './quux';",
      " ",
      " export function main() {",
    ].join("\n");

    expect(parseDiffStats(patch)).toEqual({ added: 3, removed: 2 });
  });

  it("returns { added: 0, removed: 0 } for an empty string", () => {
    expect(parseDiffStats("")).toEqual({ added: 0, removed: 0 });
  });

  it("counts correctly for a new-file diff (all additions)", () => {
    const patch = [
      "--- /dev/null",
      "+++ b/src/new-file.ts",
      "@@ -0,0 +1,4 @@",
      "+export function hello() {",
      '+  return "world";',
      "+}",
      "+",
    ].join("\n");

    expect(parseDiffStats(patch)).toEqual({ added: 4, removed: 0 });
  });

  it("counts correctly for a deletion-only diff", () => {
    const patch = [
      "--- a/src/old-file.ts",
      "+++ b/src/old-file.ts",
      "@@ -1,5 +1,2 @@",
      " import { foo } from './foo';",
      "-import { bar } from './bar';",
      "-import { baz } from './baz';",
      "-import { qux } from './qux';",
      " ",
    ].join("\n");

    expect(parseDiffStats(patch)).toEqual({ added: 0, removed: 3 });
  });

  it("returns { added: 0, removed: 0 } for malformed input (not a diff)", () => {
    const notADiff = "This is just some random text\nwith multiple lines\nbut no diff markers.";
    expect(parseDiffStats(notADiff)).toEqual({ added: 0, removed: 0 });
  });
});
