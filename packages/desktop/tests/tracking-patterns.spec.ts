import { describe, expect, it } from "vitest";
import { parseGitattributes } from "../src/hooks/useTrackedPatterns";
import { matchesAny } from "../src/lib/glob";

describe("tracking scope patterns", () => {
  it.each([
    ['"/data\\\\[1\\\\].bin"', "data[1].bin", "data1.bin"],
    ['"/with spaces/海.bin"', "with spaces/海.bin", "copy/with spaces/海.bin"],
    ['"/caf\\303\\251.bin"', "café.bin", "cafe.bin"],
    ['"/a\\\"b.bin"', 'a"b.bin', "ab.bin"],
    ['"/line\\012\\011\\177.bin"', "line\n\t\u007f.bin", "line.bin"],
    ['"/weights"', "weights", "nested/weights"],
    ["*.bin", ".hidden/nested/data.bin", "data.bin.backup"],
  ])("resolves %s to the selected path", (token, selected, other) => {
    const patterns = parseGitattributes(`${token} filter=crab -text\n`).map(rule => rule.pattern);
    expect([matchesAny(selected, patterns), matchesAny(other, patterns)]).toEqual([true, false]);
  });

  it("rejects malformed quoted rules visibly", () => {
    expect(() => parseGitattributes('"/broken\\q.bin" filter=lfs\n')).toThrow("Invalid escape");
  });
});
