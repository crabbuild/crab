import { describe, expect, it } from "vitest";
import { replaceSelectedLines } from "./pull-review-model";

describe("inline review suggestion replacement", () => {
  it("replaces an inclusive LF range and preserves the final newline", () => {
    expect(replaceSelectedLines("one\ntwo\nthree\n", 2, 2, "TWO")).toBe(
      "one\nTWO\nthree\n",
    );
  });

  it("preserves CRLF files and normalizes replacement separators", () => {
    expect(replaceSelectedLines("one\r\ntwo\r\n", 2, 2, "TWO\n2")).toBe(
      "one\r\nTWO\r\n2\r\n",
    );
  });

  it("allows empty replacement to delete selected lines", () => {
    expect(replaceSelectedLines("one\ntwo\nthree", 2, 2, "")).toBe(
      "one\nthree",
    );
    expect(replaceSelectedLines("one\ntwo\n", 2, 2, "")).toBe("one\n");
    expect(replaceSelectedLines("one\n", 1, 1, "")).toBe("");
  });

  it("uses the replacement newline state for the final logical line", () => {
    expect(replaceSelectedLines("one\ntwo\n", 2, 2, "TWO")).toBe("one\nTWO\n");
    expect(replaceSelectedLines("one\ntwo\n", 2, 2, "")).toBe("one\n");
    expect(replaceSelectedLines("one\ntwo", 2, 2, "TWO\n")).toBe("one\nTWO\n");
  });

  it("supports Unicode and files without a final newline", () => {
    expect(replaceSelectedLines("😀\ncafé", 1, 1, "🐚")).toBe("🐚\ncafé");
  });

  it.each([
    [0, 1],
    [2, 1],
    [1, 3],
  ])("rejects an out-of-bounds range (%i, %i)", (start, end) => {
    expect(() => replaceSelectedLines("one\ntwo", start, end, "new")).toThrow(
      "outside the file",
    );
  });
});
