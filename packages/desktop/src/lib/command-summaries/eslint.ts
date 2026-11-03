import type { CommandParser } from "./index";

const CMD_PATTERN = /^(eslint|prettier)\b/;

/**
 * Parses output from eslint and prettier.
 * Extracts error/warning counts from the standard summary line.
 */
export const eslintParser: CommandParser = (cmd, output) => {
  if (!CMD_PATTERN.test(cmd.trim())) {
    return { matched: false };
  }

  const tool = cmd.trim().match(CMD_PATTERN)![1];

  if (tool === "prettier") {
    return parsePrettierOutput(output);
  }

  return parseEslintOutput(output);
};

function parseEslintOutput(output: string): ReturnType<typeof eslintParser> {
  // ESLint summary line: "✖ 5 problems (3 errors, 2 warnings)"
  // Also handles singular: "✖ 2 problems (1 error, 1 warning)"
  // And non-standard plurals: "✖ 2 problems (1 errors, 1 warnings)"
  const summaryMatch = output.match(
    /(\d+)\s+problems?\s*\((\d+)\s+errors?,\s*(\d+)\s+warnings?\)/i,
  );
  if (summaryMatch) {
    const errors = parseInt(summaryMatch[2], 10);
    const warnings = parseInt(summaryMatch[3], 10);
    const status = errors > 0 ? "✗" : warnings > 0 ? "⚠" : "✓";
    const badge = `${status} ${errors} errors, ${warnings} warnings`;
    return {
      matched: true,
      summary: { icon: "⚠️", badge },
    };
  }

  // Alternative: just error/warning counts on separate lines
  const errorMatch = output.match(/(\d+)\s+errors?/i);
  const warningMatch = output.match(/(\d+)\s+warnings?/i);
  if (errorMatch || warningMatch) {
    const errors = parseInt(errorMatch?.[1] || "0", 10);
    const warnings = parseInt(warningMatch?.[1] || "0", 10);
    const status = errors > 0 ? "✗" : warnings > 0 ? "⚠" : "✓";
    const badge = `${status} ${errors} errors, ${warnings} warnings`;
    return {
      matched: true,
      summary: { icon: "⚠️", badge },
    };
  }

  // No problems found — clean run
  if (/no\s+problems/i.test(output) || output.trim() === "") {
    return {
      matched: true,
      summary: { icon: "⚠️", badge: "✓ no issues" },
    };
  }

  return { matched: false };
}

function parsePrettierOutput(output: string): ReturnType<typeof eslintParser> {
  // Prettier --check: "Checking formatting...\nAll matched the configured style."
  if (/all\s+(files\s+)?matched/i.test(output)) {
    return {
      matched: true,
      summary: { icon: "⚠️", badge: "✓ all formatted" },
    };
  }

  // Prettier --check failure lists files that need formatting
  const fileLines = output
    .split("\n")
    .filter((l) => l.trim().length > 0 && !l.startsWith("Checking"));
  if (fileLines.length > 0) {
    const count = fileLines.length;
    return {
      matched: true,
      summary: {
        icon: "⚠️",
        badge: `⚠ ${count} file${count !== 1 ? "s" : ""} need formatting`,
      },
    };
  }

  return { matched: false };
}
