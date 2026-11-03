import type { CommandParser } from "./index";

const CMD_PATTERN = /^(find|rg|grep|ag)\b/;

/**
 * Parses output from find, rg (ripgrep), grep, and ag (the silver searcher).
 * Extracts match count and unique file count.
 */
export const findRgParser: CommandParser = (cmd, output) => {
  const match = cmd.trim().match(CMD_PATTERN);
  if (!match) {
    return { matched: false };
  }

  const tool = match[1];
  const lines = output.split("\n").filter((l) => l.length > 0);

  if (lines.length === 0) {
    return { matched: false };
  }

  if (tool === "find") {
    return parseFindOutput(lines);
  }

  // rg, grep, ag all produce file:line:content or file:content lines
  return parseSearchOutput(lines);
};

function parseFindOutput(
  lines: string[],
): ReturnType<typeof findRgParser> {
  const count = lines.length;
  const uniqueFiles = new Set(lines.map((l) => l.trim())).size;
  const badge = `${count} result${count !== 1 ? "s" : ""} in ${uniqueFiles} file${uniqueFiles !== 1 ? "s" : ""}`;
  return {
    matched: true,
    summary: { icon: "🔍", badge },
  };
}

function parseSearchOutput(
  lines: string[],
): ReturnType<typeof findRgParser> {
  const matchCount = lines.length;
  const files = new Set<string>();

  for (const line of lines) {
    // rg/grep/ag format: "file:line:content" or "file:content"
    const colonIdx = line.indexOf(":");
    if (colonIdx > 0) {
      files.add(line.slice(0, colonIdx));
    } else {
      // Plain filename (e.g. grep -l)
      files.add(line.trim());
    }
  }

  const fileCount = files.size;
  const badge = `${matchCount} match${matchCount !== 1 ? "es" : ""} in ${fileCount} file${fileCount !== 1 ? "s" : ""}`;
  return {
    matched: true,
    summary: { icon: "🔍", badge },
  };
}
