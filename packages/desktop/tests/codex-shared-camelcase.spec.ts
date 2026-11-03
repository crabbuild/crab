/**
 * Regression test for the field-name mismatch between the Codex
 * app-server wire format (camelCase) and the parser's expectations.
 *
 * Bug: tool cards (Bash, Edit, etc.) appeared in "running" state but
 * never transitioned to "completed". Root cause: parseCodexItem read
 * snake_case fields (aggregated_output, exit_code, duration_ms) but
 * codex app-server emits camelCase (aggregatedOutput, exitCode,
 * durationMs) per the v2 protocol's `#[serde(rename_all = "camelCase")]`
 * on every item variant.
 *
 * Protocol source of truth:
 *   https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/item.rs
 *   https://openai-codex.mintlify.app/api/items
 */

import { describe, it, expect } from "vitest";
import {
  parseCodexEvents,
  extractUsageEvent,
  type CodexParseState,
} from "../electron/codex-shared";

function makeState(): CodexParseState {
  return { lastAgentMessage: "" };
}

describe("parseCodexItem — camelCase wire format (app-server)", () => {
  it("parses a completed commandExecution with camelCase fields and produces a tool_result with output and exit code", () => {
    // This is what `codex app-server` actually sends on the wire for a
    // simple `du -sh .` command that succeeds. Field names are camelCase
    // because every ThreadItem variant carries
    // `#[serde(rename_all = "camelCase")]` on the Rust side.
    const raw = {
      method: "item/completed",
      params: {
        threadId: "thr_123",
        turnId: "turn_456",
        item: {
          type: "commandExecution",
          id: "item_cmd",
          command: "du -sh .",
          cwd: "/Users/me/repo",
          processId: "pty-1",
          status: "completed",
          commandActions: [{ type: "unknown", command: "du -sh ." }],
          aggregatedOutput: " 94M\t.\n",
          exitCode: 0,
          durationMs: 1234,
        },
      },
    };

    const events = parseCodexEvents(raw, makeState());
    const result = events.find((e) => e.type === "tool_result");
    expect(result).toBeDefined();
    expect(result!.tool).toBe("Bash");
    expect(result!.itemId).toBe("item_cmd");
    // Output must include the aggregated stdout — without it, the
    // BashCard renderer's `hasOutput` stays false and the card never
    // flips out of the "running" badge.
    expect(result!.output).toContain("94M");
    // Duration should be rendered too.
    expect(result!.output).toContain("Duration: 1.2s");
    // Final marker so the renderer knows this is the terminal event.
    expect((result!.input as any).final).toBe(true);
    expect((result!.input as any).exitCode).toBe(0);
    expect((result!.input as any).durationMs).toBe(1234);
    expect((result!.input as any).status).toBe("completed");
  });

  it("renders a failing commandExecution's stderr and non-zero exit code", () => {
    const raw = {
      method: "item/completed",
      params: {
        item: {
          type: "commandExecution",
          id: "item_cmd2",
          command: "false",
          cwd: "/tmp",
          status: "failed",
          commandActions: [],
          aggregatedOutput: "boom\n",
          exitCode: 1,
          durationMs: 12,
        },
      },
    };
    const events = parseCodexEvents(raw, makeState());
    const result = events.find((e) => e.type === "tool_result");
    expect(result).toBeDefined();
    expect(result!.output).toContain("boom");
    expect(result!.output).toContain("Exit code: 1");
    expect((result!.input as any).exitCode).toBe(1);
    expect((result!.input as any).status).toBe("failed");
  });

  it("flips a successful commandExecution with no stdout to a non-empty output so the card stops showing as running", () => {
    // Real-world example: `mkdir foo` produces no stdout/stderr but the
    // card must still leave the running state. Before the fix the
    // output was "" and BashCard's `hasOutput` stayed false forever.
    const raw = {
      method: "item/completed",
      params: {
        item: {
          type: "commandExecution",
          id: "item_cmd3",
          command: "mkdir foo",
          cwd: "/tmp",
          status: "completed",
          commandActions: [],
          aggregatedOutput: "",
          exitCode: 0,
          durationMs: 5,
        },
      },
    };
    const events = parseCodexEvents(raw, makeState());
    const result = events.find((e) => e.type === "tool_result");
    expect(result).toBeDefined();
    expect(result!.output).toBeTruthy();
    expect((result!.input as any).final).toBe(true);
  });

  it("still parses legacy snake_case payloads (backward compatibility)", () => {
    const raw = {
      method: "item/completed",
      params: {
        item: {
          type: "commandExecution",
          id: "item_cmd4",
          command: "echo hi",
          cwd: "/tmp",
          status: "completed",
          command_actions: [],
          aggregated_output: "hi\n",
          exit_code: 0,
          duration_ms: 7,
        },
      },
    };
    const events = parseCodexEvents(raw, makeState());
    const result = events.find((e) => e.type === "tool_result");
    expect(result).toBeDefined();
    expect(result!.output).toContain("hi");
    expect((result!.input as any).durationMs).toBe(7);
  });

  it("parses a fileChange item.completed with camelCase changes whose `kind` is the v2 internally-tagged enum", () => {
    // Wire shape per v2 PatchChangeKind:
    //   {"type": "add"} | {"type": "delete"} | {"type": "update", "movePath": null}
    const raw = {
      method: "item/completed",
      params: {
        item: {
          type: "fileChange",
          id: "item_file",
          status: "completed",
          changes: [
            {
              path: "src/new.rs",
              kind: { type: "add" },
              diff: "+ added\n",
            },
            {
              path: "src/old.rs",
              kind: { type: "delete" },
              diff: "- removed\n",
            },
            {
              path: "src/edit.rs",
              kind: { type: "update", movePath: null },
              diff: "@@ -1 +1 @@\n",
            },
          ],
        },
      },
    };
    const events = parseCodexEvents(raw, makeState());
    expect(events).toHaveLength(3);
    expect(events[0].type).toBe("file_edit");
    expect(events[0].tool).toBe("Write");
    expect((events[0].input as any).kind).toBe("add");
    expect(events[1].tool).toBe("FileDelete");
    expect((events[1].input as any).kind).toBe("delete");
    expect(events[2].tool).toBe("Edit");
    expect((events[2].input as any).kind).toBe("update");
  });

  it("parses an item.started for fileChange and derives a path from changes[0]", () => {
    const raw = {
      method: "item/started",
      params: {
        item: {
          type: "fileChange",
          id: "item_file2",
          status: "inProgress",
          changes: [{ path: "src/lib.rs", kind: { type: "update" }, diff: "" }],
        },
      },
    };
    const events = parseCodexEvents(raw, makeState());
    expect(events).toHaveLength(1);
    expect(events[0].type).toBe("tool_use");
    expect(events[0].tool).toBe("Write");
    expect((events[0].input as any).file_path).toBe("src/lib.rs");
  });

  it("parses an mcpToolCall item.completed with camelCase durationMs", () => {
    const raw = {
      method: "item/completed",
      params: {
        item: {
          type: "mcpToolCall",
          id: "item_mcp",
          server: "github",
          tool: "create_issue",
          status: "completed",
          arguments: { title: "x" },
          result: { url: "..." },
          durationMs: 2222,
        },
      },
    };
    const events = parseCodexEvents(raw, makeState());
    const r = events.find((e) => e.type === "tool_result");
    expect(r).toBeDefined();
    expect(r!.tool).toBe("create_issue");
    expect((r!.input as any).durationMs).toBe(2222);
    expect((r!.input as any).server).toBe("github");
    expect((r!.input as any).final).toBe(true);
  });
});

describe("extractUsageEvent — camelCase wire format", () => {
  it("reads camelCase keys (inputTokens / outputTokens / cachedInputTokens / reasoningOutputTokens)", () => {
    const usage = {
      inputTokens: 100,
      outputTokens: 40,
      cachedInputTokens: 20,
      reasoningOutputTokens: 5,
    };
    const event = extractUsageEvent(usage);
    expect(event).not.toBeNull();
    expect(event!.usage).toEqual({
      inputTokens: 100,
      outputTokens: 45, // base + reasoning
      cacheCreationInputTokens: 0,
      cacheReadInputTokens: 20,
    });
  });

  it("still reads legacy snake_case keys", () => {
    const usage = {
      input_tokens: 100,
      output_tokens: 40,
      cached_input_tokens: 20,
      reasoning_output_tokens: 5,
    };
    const event = extractUsageEvent(usage);
    expect(event).not.toBeNull();
    expect(event!.usage.inputTokens).toBe(100);
    expect(event!.usage.outputTokens).toBe(45);
    expect(event!.usage.cacheReadInputTokens).toBe(20);
  });
});
