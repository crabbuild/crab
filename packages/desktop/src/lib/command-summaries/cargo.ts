/**
 * Parser for cargo test/build/check commands.
 * Extracts pass/fail counts and duration from cargo output.
 */

import type { CommandParser } from "./index";

const CARGO_CMD_RE =
  /^cargo\s+(?:test|build|check)/;

/** Matches "test result: ok. 12 passed; 0 failed; 0 ignored;" */
const CARGO_TEST_RESULT_RE =
  /test result:\s*(\w+)\.\s*(\d+)\s+passed;\s*(\d+)\s+failed/im;

/** Matches "Finished `test` profile" or "Compiling crab v0.1.0" */
const CARGO_FINISHED_RE =
  /Finished.*in\s+(\d+(?:\.\d+)?s)/im;

/** Matches "Compiling N crates" or individual "Compiling foo v1.0" */
const CARGO_COMPILING_RE =
  /Compiling\s+(\d+)\s+crate/im;

export const cargoParser: CommandParser = (cmd, output) => {
  if (!CARGO_CMD_RE.test(cmd.trim())) {
    return { matched: false };
  }

  const testMatch = output.match(CARGO_TEST_RESULT_RE);
  if (testMatch) {
    const status = testMatch[1]; // "ok" or "FAILED"
    const passed = parseInt(testMatch[2], 10) || 0;
    const failed = parseInt(testMatch[3], 10) || 0;
    const icon = status === "ok" ? "✓" : "✗";

    const parts = [`${icon} ${passed} passed`];
    if (failed > 0) parts.push(`${failed} failed`);

    const durMatch = output.match(CARGO_FINISHED_RE);
    if (durMatch) parts.push(durMatch[1]);

    return {
      matched: true,
      summary: {
        icon: "🦀",
        badge: `cargo test  ${parts.join("  ")}`,
      },
    };
  }

  // cargo build / cargo check without test results
  const finishedMatch = output.match(CARGO_FINISHED_RE);
  const compilingMatch = output.match(CARGO_COMPILING_RE);

  if (finishedMatch || compilingMatch) {
    const parts: string[] = [];
    if (compilingMatch) parts.push(`${compilingMatch[1]} crates`);
    if (finishedMatch) parts.push(finishedMatch[1]);

    const verb = cmd.trim().includes("build") ? "cargo build" : "cargo check";
    return {
      matched: true,
      summary: {
        icon: "🦀",
        badge: `${verb}  ✓ ${parts.join("  ")}`,
      },
    };
  }

  return { matched: false };
};
