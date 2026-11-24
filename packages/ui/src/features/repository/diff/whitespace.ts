import { parseDiffFromFile, type FileContents } from "@pierre/diffs";

export function diffIgnoringWhitespace(
  oldFile: FileContents,
  newFile: FileContents,
) {
  // Pierre's full-file model keeps one array entry per LF-delimited line.
  const lines = (text: string) => (text ? text.split(/(?<=\n)/) : []);
  const oldLines = lines(oldFile.contents);
  const newLines = lines(newFile.contents);
  const normalized = (file: FileContents, rows: string[]) => ({
    ...file,
    // The prefix keeps even a whitespace-only unterminated line in the patch.
    contents: rows
      .map((line) => `:${line.replace(/[\t\v\f\r ]/g, "")}`)
      .join(""),
    cacheKey: file.cacheKey && `ignore-whitespace:${file.cacheKey}`,
  });
  const diff = parseDiffFromFile(
    normalized(oldFile, oldLines),
    normalized(newFile, newLines),
  );
  // Match lines with normalized text, then render the exact source on each side.
  return { ...diff, deletionLines: oldLines, additionLines: newLines };
}
