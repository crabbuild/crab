/**
 * Command summary parser registry.
 *
 * Each parser is a pure function that takes a command string and its output,
 * returning either a matched summary or { matched: false }. The registry
 * is walked in order; the first match wins.
 */

export interface CommandSummary {
  /** Tool icon to render alongside the badge, e.g. "📦", "🦀". */
  icon: string;
  /** Concise badge text, ≤ 80 chars. */
  badge: string;
}

export type CommandParser = (
  cmd: string,
  output: string,
) => { matched: true; summary: CommandSummary } | { matched: false };

import { npmTestParser } from "./npm-test";
import { cargoParser } from "./cargo";
import { gitStatusParser } from "./git-status";
import { findRgParser } from "./find-rg";
import { eslintParser } from "./eslint";

export { npmTestParser, cargoParser, gitStatusParser, findRgParser, eslintParser };

export const commandParsers: CommandParser[] = [
  npmTestParser,
  cargoParser,
  gitStatusParser,
  findRgParser,
  eslintParser,
];

/**
 * Walk the parser registry and return the first matching summary, or null.
 */
export function summariseCommand(
  cmd: string,
  output: string,
): CommandSummary | null {
  for (const p of commandParsers) {
    const r = p(cmd, output);
    if (r.matched) return r.summary;
  }
  return null;
}
