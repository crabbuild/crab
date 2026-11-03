/**
 * Tests for command summary parsers.
 *
 * Validates: Requirements CHAT7.1–7.6
 * Design: D12
 */

import { describe, it, expect } from "vitest";
import {
  npmTestParser,
  cargoParser,
  gitStatusParser,
  findRgParser,
  eslintParser,
  summariseCommand,
} from "../src/lib/command-summaries";

// ---------------------------------------------------------------------------
// npmTestParser
// ---------------------------------------------------------------------------

describe("npmTestParser", () => {
  it("parses a vitest summary with pass count and duration", () => {
    const cmd = "npm test";
    const output = [
      " ✓ src/utils.spec.ts (3 tests) 12ms",
      " ✓ src/lib.spec.ts (2 tests) 8ms",
      "",
      " Tests  5 passed (5)",
      " Duration  1.23s",
    ].join("\n");

    const result = npmTestParser(cmd, output);
    expect(result.matched).toBe(true);
    if (result.matched) {
      expect(result.summary.icon).toBe("📦");
      expect(result.summary.badge).toContain("5 passed");
      expect(result.summary.badge).toContain("1.23s");
    }
  });

  it("returns no match for an unrelated command", () => {
    const result = npmTestParser("npm install", "added 120 packages");
    expect(result.matched).toBe(false);
  });

  it("returns no match when output is truncated (no summary line)", () => {
    const cmd = "npm test";
    const output = [
      " ✓ src/utils.spec.ts (3 tests) 12ms",
      " ✓ src/lib.spec.ts (2 tests) 8ms",
      // Missing the "Tests  N passed" summary line
    ].join("\n");

    const result = npmTestParser(cmd, output);
    expect(result.matched).toBe(false);
  });

  it("matches pnpm test and yarn test variants", () => {
    const output = "Tests  10 passed (10)\nDuration  2.5s";
    expect(npmTestParser("pnpm test", output).matched).toBe(true);
    expect(npmTestParser("yarn test", output).matched).toBe(true);
    expect(npmTestParser("npm run test", output).matched).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// cargoParser
// ---------------------------------------------------------------------------

describe("cargoParser", () => {
  it("parses cargo test output with pass/fail counts", () => {
    const cmd = "cargo test";
    const output = [
      "   Compiling crab v0.1.0 (/Users/dev/crab)",
      "    Finished `test` profile in 4.2s",
      "     Running unittests src/main.rs",
      "",
      "running 24 tests",
      "test core::config::tests::parse_config ... ok",
      "test engine::chunking::tests::gearhash_boundary ... ok",
      "",
      "test result: ok. 24 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 1.82s",
    ].join("\n");

    const result = cargoParser(cmd, output);
    expect(result.matched).toBe(true);
    if (result.matched) {
      expect(result.summary.icon).toBe("🦀");
      expect(result.summary.badge).toContain("24 passed");
      expect(result.summary.badge).not.toContain("failed");
    }
  });

  it("returns no match for an unrelated command", () => {
    const result = cargoParser("cargo add serde", "Adding serde v1.0");
    expect(result.matched).toBe(false);
  });

  it("returns no match when output is truncated (compilation only, no result)", () => {
    const cmd = "cargo test";
    const output = [
      "   Compiling crab v0.1.0",
      // Stream cut off before test result line
    ].join("\n");

    const result = cargoParser(cmd, output);
    expect(result.matched).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// gitStatusParser
// ---------------------------------------------------------------------------

describe("gitStatusParser", () => {
  it("parses porcelain output into modified/untracked counts", () => {
    const cmd = "git status --short";
    const output = [
      " M src/lib.ts",
      " M src/utils.ts",
      "?? new-file.txt",
      "?? another-new.ts",
      "?? tmp/",
    ].join("\n");

    const result = gitStatusParser(cmd, output);
    expect(result.matched).toBe(true);
    if (result.matched) {
      expect(result.summary.icon).toBe("📋");
      expect(result.summary.badge).toContain("2 modified");
      expect(result.summary.badge).toContain("3 untracked");
    }
  });

  it("returns no match for a non-status git command", () => {
    const result = gitStatusParser("git log --oneline", "abc1234 Initial commit");
    expect(result.matched).toBe(false);
  });

  it("handles empty output (clean working tree)", () => {
    const cmd = "git status -s";
    const output = "";

    const result = gitStatusParser(cmd, output);
    expect(result.matched).toBe(true);
    if (result.matched) {
      expect(result.summary.badge).toContain("clean");
    }
  });
});

// ---------------------------------------------------------------------------
// findRgParser
// ---------------------------------------------------------------------------

describe("findRgParser", () => {
  it("parses rg output with match count and file count", () => {
    const cmd = "rg TODO src/";
    const output = [
      "src/main.ts:12:// TODO: refactor this",
      "src/main.ts:45:// TODO: add error handling",
      "src/utils.ts:8:// TODO: optimize",
      "src/lib.ts:100:// TODO: document",
    ].join("\n");

    const result = findRgParser(cmd, output);
    expect(result.matched).toBe(true);
    if (result.matched) {
      expect(result.summary.icon).toBe("🔍");
      expect(result.summary.badge).toContain("4 matches");
      expect(result.summary.badge).toContain("3 files");
    }
  });

  it("returns no match for an unrelated command", () => {
    const result = findRgParser("ls -la", "total 48\ndrwxr-xr-x");
    expect(result.matched).toBe(false);
  });

  it("returns no match when output is empty (no results)", () => {
    const cmd = "rg nonexistent";
    const output = "";

    const result = findRgParser(cmd, output);
    expect(result.matched).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// eslintParser
// ---------------------------------------------------------------------------

describe("eslintParser", () => {
  it("parses eslint summary with error and warning counts", () => {
    const cmd = "eslint src/";
    const output = [
      "/Users/dev/src/main.ts",
      "  12:5  error  Unexpected any  @typescript-eslint/no-explicit-any",
      "  45:1  warning  Unexpected console statement  no-console",
      "",
      "✖ 2 problems (1 errors, 1 warnings)",
    ].join("\n");

    const result = eslintParser(cmd, output);
    expect(result.matched).toBe(true);
    if (result.matched) {
      expect(result.summary.icon).toBe("⚠️");
      expect(result.summary.badge).toContain("1 errors");
      expect(result.summary.badge).toContain("1 warnings");
    }
  });

  it("returns no match for an unrelated command", () => {
    const result = eslintParser("tsc --noEmit", "error TS2345: ...");
    expect(result.matched).toBe(false);
  });

  it("returns no match when output has no parseable summary", () => {
    const cmd = "eslint src/";
    const output = [
      "Oops! Something went wrong!",
      "ESLint couldn't find a configuration file.",
    ].join("\n");

    const result = eslintParser(cmd, output);
    expect(result.matched).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// summariseCommand (top-level registry)
// ---------------------------------------------------------------------------

describe("summariseCommand", () => {
  it("returns the first matching parser result", () => {
    const result = summariseCommand(
      "npm test",
      "Tests  3 passed (3)\nDuration  0.8s",
    );
    expect(result).not.toBeNull();
    expect(result!.icon).toBe("📦");
    expect(result!.badge).toContain("3 passed");
  });

  it("returns null when no parser matches", () => {
    const result = summariseCommand("echo hello", "hello");
    expect(result).toBeNull();
  });

  it("falls through to the next parser when the first does not match", () => {
    // cargo test should be matched by cargoParser, not npmTestParser
    const result = summariseCommand(
      "cargo test",
      "test result: ok. 7 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.5s",
    );
    expect(result).not.toBeNull();
    expect(result!.icon).toBe("🦀");
    expect(result!.badge).toContain("7 passed");
    expect(result!.badge).toContain("1 failed");
  });
});
