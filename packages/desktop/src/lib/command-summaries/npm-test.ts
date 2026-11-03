import type { CommandParser } from "./index";

const CMD_PATTERN = /^(npm\s+test|pnpm\s+test|yarn\s+test|npm\s+run\s+test)/;

/**
 * Parses output from npm/pnpm/yarn test runners.
 * Looks for common test-result summary lines produced by Jest, Vitest, Mocha, etc.
 */
export const npmTestParser: CommandParser = (cmd, output) => {
  if (!CMD_PATTERN.test(cmd.trim())) {
    return { matched: false };
  }

  // Vitest style: "Tests  5 passed (5)" or "Tests  3 failed | 5 passed (8)"
  const vitestMatch = output.match(
    /Tests\s+(?:(\d+)\s+failed\s*\|\s*)?(\d+)\s+passed/i,
  );
  if (vitestMatch) {
    const failed = parseInt(vitestMatch[1] || "0", 10);
    const passed = parseInt(vitestMatch[2], 10);
    const duration = parseDuration(output);
    const status = failed > 0 ? "✗" : "✓";
    const badge = failed > 0
      ? `${status} ${passed} passed, ${failed} failed${duration ? `  ${duration}` : ""}`
      : `${status} ${passed} passed${duration ? `  ${duration}` : ""}`;
    return {
      matched: true,
      summary: { icon: "📦", badge },
    };
  }

  // Jest style: "Tests:  3 passed, 1 failed, 4 total"
  const jestMatch = output.match(
    /Tests:\s+(?:(\d+)\s+failed,?\s*)?(?:(\d+)\s+passed)?.*?(\d+)\s+total/i,
  );
  if (jestMatch) {
    const failed = parseInt(jestMatch[1] || "0", 10);
    const passed = parseInt(jestMatch[2] || "0", 10);
    const duration = parseDuration(output);
    const status = failed > 0 ? "✗" : "✓";
    const badge = failed > 0
      ? `${status} ${passed} passed, ${failed} failed${duration ? `  ${duration}` : ""}`
      : `${status} ${passed} passed${duration ? `  ${duration}` : ""}`;
    return {
      matched: true,
      summary: { icon: "📦", badge },
    };
  }

  // Mocha style: "N passing (Xs)" or "N passing\n  M failing"
  const mochaPass = output.match(/(\d+)\s+passing/i);
  const mochaFail = output.match(/(\d+)\s+failing/i);
  if (mochaPass) {
    const passed = parseInt(mochaPass[1], 10);
    const failed = parseInt(mochaFail?.[1] || "0", 10);
    const duration = parseDuration(output);
    const status = failed > 0 ? "✗" : "✓";
    const badge = failed > 0
      ? `${status} ${passed} passed, ${failed} failed${duration ? `  ${duration}` : ""}`
      : `${status} ${passed} passed${duration ? `  ${duration}` : ""}`;
    return {
      matched: true,
      summary: { icon: "📦", badge },
    };
  }

  // Generic: look for "N tests passed" or "N passed"
  const genericPassed = output.match(/(\d+)\s+(?:tests?\s+)?passed/i);
  const genericFailed = output.match(/(\d+)\s+(?:tests?\s+)?failed/i);
  if (genericPassed || genericFailed) {
    const passed = parseInt(genericPassed?.[1] || "0", 10);
    const failed = parseInt(genericFailed?.[1] || "0", 10);
    const duration = parseDuration(output);
    const status = failed > 0 ? "✗" : "✓";
    const badge = failed > 0
      ? `${status} ${passed} passed, ${failed} failed${duration ? `  ${duration}` : ""}`
      : `${status} ${passed} passed${duration ? `  ${duration}` : ""}`;
    return {
      matched: true,
      summary: { icon: "📦", badge },
    };
  }

  return { matched: false };
};

/** Extracts a duration string from test output. */
function parseDuration(output: string): string | null {
  // "Duration  1.23s" (Vitest)
  const vitestTime = output.match(/Duration\s+([\d.]+\s*m?s)/i);
  if (vitestTime) return vitestTime[1];

  // "Time:  3.456 s" (Jest)
  const jestTime = output.match(/Time:\s+([\d.]+\s*m?s)/i);
  if (jestTime) return jestTime[1];

  // "(Xs)" or "(Xms)" inline with "passing"
  const inlineTime = output.match(/passing\s*\(([\d.]+\s*m?s)\)/i);
  if (inlineTime) return inlineTime[1];

  return null;
}
