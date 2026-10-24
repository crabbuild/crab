function normalizeSeparators(value: string) {
  return value.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
}

function separatorFor(value: string) {
  return value.includes("\r\n") ? "\r\n" : "\n";
}

/** Replace an inclusive one-based range while preserving the file's newline contract. */
export function replaceSelectedLines(
  original: string,
  startLine: number,
  endLine: number,
  replacement: string,
): string {
  const separator = separatorFor(original);
  const normalized = normalizeSeparators(original);
  const hadFinalNewline = normalized.endsWith("\n");
  const lines = normalized.split("\n");
  if (hadFinalNewline) lines.pop();
  if (
    !Number.isInteger(startLine) ||
    !Number.isInteger(endLine) ||
    startLine < 1 ||
    endLine < startLine ||
    endLine > lines.length
  ) {
    throw new Error("The selected review range is outside the file");
  }

  const normalizedReplacement = normalizeSeparators(replacement);
  const replacementHadFinalNewline = normalizedReplacement.endsWith("\n");
  const replacementLines =
    normalizedReplacement.length === 0 ? [] : normalizedReplacement.split("\n");
  if (replacementHadFinalNewline) replacementLines.pop();
  const selectsFinalLine = endLine === lines.length;
  lines.splice(startLine - 1, endLine - startLine + 1, ...replacementLines);
  if (lines.length === 0) return "";

  const finalNewline =
    hadFinalNewline || (selectsFinalLine && replacementHadFinalNewline);
  const result = lines.join("\n");
  return `${result}${finalNewline ? "\n" : ""}`.replaceAll("\n", separator);
}
