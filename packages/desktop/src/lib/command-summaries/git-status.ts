/**
 * Parser for `git status --short` / `git status -s` commands.
 * Extracts modified/untracked/added counts from porcelain output.
 */

import type { CommandParser } from "./index";

const GIT_STATUS_RE =
  /^git\s+status\s+(?:--short|-s)/;

export const gitStatusParser: CommandParser = (cmd, output) => {
  if (!GIT_STATUS_RE.test(cmd.trim())) {
    return { matched: false };
  }

  const lines = output.split("\n").filter((l) => l.trim().length > 0);
  if (lines.length === 0) {
    return {
      matched: true,
      summary: {
        icon: "📋",
        badge: "git status  clean",
      },
    };
  }

  let modified = 0;
  let untracked = 0;
  let added = 0;
  let deleted = 0;

  for (const line of lines) {
    const code = line.slice(0, 2);
    if (code === "??") {
      untracked++;
    } else if (code.includes("M")) {
      modified++;
    } else if (code.includes("A")) {
      added++;
    } else if (code.includes("D")) {
      deleted++;
    } else {
      // Other statuses (renamed, copied, etc.) count as modified
      modified++;
    }
  }

  const parts: string[] = [];
  if (modified > 0) parts.push(`${modified} modified`);
  if (added > 0) parts.push(`${added} added`);
  if (deleted > 0) parts.push(`${deleted} deleted`);
  if (untracked > 0) parts.push(`${untracked} untracked`);

  return {
    matched: true,
    summary: {
      icon: "📋",
      badge: `git status  ${parts.join(", ")}`,
    },
  };
};
