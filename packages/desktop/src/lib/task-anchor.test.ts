import { describe, expect, it } from "vitest";
import { parseAnchor, formatAnchor } from "./task-anchor";

describe("parseAnchor", () => {
  it("parses a file anchor", () => {
    const result = parseAnchor("file:src/main.ts:42");
    expect(result).toEqual({ type: "file", path: "src/main.ts", line: 42 });
  });

  it("parses a file anchor with nested path", () => {
    const result = parseAnchor("file:src/lib/utils/helpers.ts:100");
    expect(result).toEqual({ type: "file", path: "src/lib/utils/helpers.ts", line: 100 });
  });

  it("parses a commit anchor", () => {
    const result = parseAnchor("commit:abc1234");
    expect(result).toEqual({ type: "commit", sha: "abc1234" });
  });

  it("parses a full-length commit SHA", () => {
    const sha = "abc1234def5678901234567890abcdef12345678";
    const result = parseAnchor(`commit:${sha}`);
    expect(result).toEqual({ type: "commit", sha });
  });

  it("returns null for empty string", () => {
    expect(parseAnchor("")).toBeNull();
  });

  it("returns null for unrecognized prefix", () => {
    expect(parseAnchor("branch:main")).toBeNull();
  });

  it("returns null for file anchor without line number", () => {
    expect(parseAnchor("file:src/main.ts")).toBeNull();
  });

  it("returns null for file anchor with non-numeric line", () => {
    expect(parseAnchor("file:src/main.ts:abc")).toBeNull();
  });

  it("returns null for commit anchor with invalid SHA characters", () => {
    expect(parseAnchor("commit:xyz!@#")).toBeNull();
  });

  it("returns null for commit anchor with empty SHA", () => {
    expect(parseAnchor("commit:")).toBeNull();
  });

  it("returns null for commit anchor with too-short SHA", () => {
    expect(parseAnchor("commit:abc")).toBeNull();
  });

  it("handles file anchor with line 0", () => {
    const result = parseAnchor("file:README.md:0");
    expect(result).toEqual({ type: "file", path: "README.md", line: 0 });
  });
});

describe("formatAnchor", () => {
  it("formats a file anchor to filename:line", () => {
    expect(formatAnchor("file:src/utils/helpers.ts:42")).toBe("helpers.ts:42");
  });

  it("formats a file anchor with simple filename", () => {
    expect(formatAnchor("file:README.md:1")).toBe("README.md:1");
  });

  it("formats a commit anchor to short SHA", () => {
    expect(formatAnchor("commit:abc1234def5678")).toBe("abc1234");
  });

  it("returns the raw string for invalid anchors", () => {
    expect(formatAnchor("invalid:stuff")).toBe("invalid:stuff");
  });

  it("formats a 7-char commit SHA without truncation", () => {
    expect(formatAnchor("commit:abc1234")).toBe("abc1234");
  });
});
