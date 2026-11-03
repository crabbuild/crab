/**
 * Unit tests for codex-shared.ts utilities.
 *
 * Validates: Requirements CA5.2
 * Design: D5, CA-P3
 */

import { describe, it, expect } from "vitest";
import {
  mapDecisionToCodex,
  stripAnsi,
  CODEX_TRACE_LINE_RE,
  CODEX_MCP_NOISE_RE,
} from "../electron/codex-shared";

describe("mapDecisionToCodex", () => {
  it.each([
    { input: "allow" as const, expected: "acceptForSession" },
    { input: "allow_once" as const, expected: "accept" },
    { input: "deny" as const, expected: "decline" },
    { input: "cancel" as const, expected: "cancel" },
  ])('maps "$input" → "$expected"', ({ input, expected }) => {
    expect(mapDecisionToCodex(input)).toBe(expected);
  });

  it("throws on an unknown decision value", () => {
    // Force an invalid value past the type system to verify runtime guard.
    expect(() => mapDecisionToCodex("bogus" as any)).toThrow("unreachable decision");
  });
});

describe("stripAnsi", () => {
  it("returns plain text unchanged", () => {
    expect(stripAnsi("hello world")).toBe("hello world");
  });

  it("strips real ESC-prefixed CSI sequences", () => {
    const input = "\x1b[2m2026-05-22T21:25:36Z\x1b[0m \x1b[31mERROR\x1b[0m rmcp::transport::worker";
    expect(stripAnsi(input)).toBe("2026-05-22T21:25:36Z ERROR rmcp::transport::worker");
  });

  it("strips bare CSI sequences (ESC byte already eaten upstream)", () => {
    // This is the shape we see in the chat — the ESC byte is gone but the
    // `[Nm` markers leak through.
    const input = "[2m2026-05-22T21:25:36.330396Z[0m [31mERROR[0m [2mrmcp::transport::worker[0m: worker quit";
    expect(stripAnsi(input)).toBe("2026-05-22T21:25:36.330396Z ERROR rmcp::transport::worker: worker quit");
  });

  it("does not eat plain bracketed text", () => {
    expect(stripAnsi("[INFO] something happened")).toBe("[INFO] something happened");
    expect(stripAnsi("array[0] = 42")).toBe("array[0] = 42");
  });
});

describe("CODEX_TRACE_LINE_RE (after stripAnsi)", () => {
  it("matches a tracing-format line with ANSI markers once stripped", () => {
    const raw = "[2m2026-05-22T21:25:36.330396Z[0m [31mERROR[0m [2mrmcp::transport::worker[0m: …";
    expect(CODEX_TRACE_LINE_RE.test(stripAnsi(raw))).toBe(true);
  });

  it("matches a plain tracing-format line", () => {
    expect(CODEX_TRACE_LINE_RE.test("2026-05-08T19:16:51.208414Z ERROR codex_api::endpoint: foo")).toBe(true);
  });

  it("does not match an ordinary error string", () => {
    expect(CODEX_TRACE_LINE_RE.test("Error: missing config")).toBe(false);
  });
});

describe("CODEX_MCP_NOISE_RE", () => {
  it("matches rmcp::transport::worker lines", () => {
    const raw = "2026-05-22T21:25:36.330396Z ERROR rmcp::transport::worker: worker quit with fatal: Transport channel closed, when AuthRequired(...)";
    expect(CODEX_MCP_NOISE_RE.test(raw)).toBe(true);
  });

  it("matches rmcp::service lines", () => {
    expect(CODEX_MCP_NOISE_RE.test("ERROR rmcp::service: failed to start")).toBe(true);
  });

  it("does not match unrelated lines that mention rmcp without the worker/service path", () => {
    expect(CODEX_MCP_NOISE_RE.test("rmcp client connected")).toBe(false);
  });
});
