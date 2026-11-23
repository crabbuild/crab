import { parseDiffFromFile } from "@pierre/diffs";
import { expect, it } from "vitest";
import { diffIgnoringWhitespace } from "./whitespace";

function file(contents: string, cacheKey: string) {
  return { name: "test.txt", contents, cacheKey };
}

it("compares whitespace-free lines while retaining the original lines and coordinates", () => {
  const before = file("a + b\r\n\nold text\n  ", "base");
  const after = file("a+b\n\nnew text\n\t", "head");
  const diff = diffIgnoringWhitespace(before, after);
  expect(diff.deletionLines).toEqual(["a + b\r\n", "\n", "old text\n", "  "]);
  expect(diff.additionLines).toEqual(["a+b\n", "\n", "new text\n", "\t"]);
  expect(
    diff.hunks
      .flatMap((hunk) => hunk.hunkContent)
      .filter((part) => part.type === "change"),
  ).toEqual([
    {
      type: "change",
      additions: 1,
      deletions: 1,
      additionLineIndex: 2,
      deletionLineIndex: 2,
    },
  ]);
  expect(diff.cacheKey).not.toBe(parseDiffFromFile(before, after).cacheKey);
});

it("keeps the physical line of an unterminated whitespace-only addition", () => {
  const diff = diffIgnoringWhitespace(file("", "empty"), file("\t", "blank"));
  expect(diff.additionLines).toEqual(["\t"]);
  expect(diff.hunks[0]).toMatchObject({
    additionStart: 1,
    additionCount: 1,
    noEOFCRAdditions: true,
  });
});

it("produces zero hunks for whitespace-only edits within existing lines", () => {
  expect(
    diffIgnoringWhitespace(
      file(" x+y\t\n", "spaced"),
      file("x + y\n", "compact"),
    ).hunks,
  ).toEqual([]);
});
