import { describe, expect, it } from "vitest";
import { ClaudeEventParser } from "../electron/claude-code-events";
import { deriveTurns } from "../src/lib/turn-derivation";

describe("Claude Code stream-json events", () => {
  it("streams initial text and deltas once, retaining complete subagent output", () => {
    const parser = new ClaudeEventParser("/project");
    const events = [
      parser.parse({ type: "stream_event", event: { type: "message_start", message: { id: "message-1" } } }),
      parser.parse({ type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "text", text: "Initial " } } }),
      parser.parse({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "text" } } }),
      parser.parse({ type: "assistant", message: { id: "message-1", content: [{ type: "text", text: "Initial text" }] } }),
      parser.parse({ type: "assistant", parent_tool_use_id: "agent-tool", message: { id: "subagent-message", content: [{ type: "text", text: "Subagent result" }] } }),
    ].flat();
    expect({ parent: events.filter(event => event.type === "text").map(event => event.content),
      child: events.filter(event => event.subagent?.event).map(event => [event.subagent?.id, event.subagent?.event?.content]) })
      .toEqual({ parent: ["Initial ", "text"], child: [["agent-tool", "Subagent result"]] });
  });

  it("deduplicates thinking and resets stream state at turn completion", () => {
    const parser = new ClaudeEventParser("/project");
    parser.parse({ type: "stream_event", event: { type: "message_start", message: { id: "thinking-message" } } });
    const thinking = parser.parse({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "Reasoning" } } });
    const complete = parser.parse({ type: "assistant", message: { id: "thinking-message", content: [{ type: "thinking", thinking: "Reasoning" }] } });
    parser.parse({ type: "result", subtype: "success", result: "Done" });
    const next = parser.parse({ type: "assistant", message: { id: "next-message", content: [{ type: "text", text: "Next turn" }] } });
    expect([...thinking, ...complete, ...next].map(event => [event.type, event.content])).toEqual([["thinking", "Reasoning"], ["text", "Next turn"]]);
  });

  it("pairs each completed block with its own stream, retaining later unstreamed blocks", () => {
    const parser = new ClaudeEventParser("");
    const events = [
      parser.parse({ type: "stream_event", event: { type: "message_start", message: { id: "multi-block" } } }),
      parser.parse({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "ha" } } }),
      parser.parse({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "ha" } } }),
      parser.parse({ type: "assistant", uuid: "block-0", message: { id: "multi-block", content: [{ type: "text", text: "haha" }] } }),
      parser.parse({ type: "assistant", uuid: "block-1", message: { id: "multi-block", content: [{ type: "text", text: "Another block" }] } }),
      parser.parse({ type: "assistant", uuid: "block-1", message: { id: "multi-block", content: [{ type: "text", text: "Another block" }] } }),
    ].flat();
    expect(events.filter(event => event.type === "text").map(event => [event.itemId, event.content])).toEqual([
      ["multi-block:0", "ha"], ["multi-block:0", "ha"], ["block-1:0", "Another block"],
    ]);
  });

  it("pairs concurrent tools and results by the official tool-use id", () => {
    const parser = new ClaudeEventParser("/project");
    const calls = parser.parse({ type: "assistant", message: { id: "tools", content: [
      { type: "tool_use", id: "read-a", name: "Read", input: { file_path: "/project/a.ts" } },
      { type: "tool_use", id: "read-b", name: "Read", input: { file_path: "/project/b.ts" } },
    ] } });
    const results = parser.parse({ type: "user", message: { content: [
      { type: "tool_result", tool_use_id: "read-b", content: "B" },
      { type: "tool_result", tool_use_id: "read-a", content: "A" },
    ] } });
    const done = parser.parse({ type: "result", subtype: "success", result: "Read both" });
    expect({ ids: [...calls, ...results].map(event => event.itemId), paths: calls.map(event => event.path), complete: deriveTurns([...calls, ...results, ...done], [])[0]?.toolGroups[0]?.complete }).toEqual({
      ids: ["read-a", "read-b", "read-b", "read-a"], paths: ["a.ts", "b.ts"], complete: true,
    });
  });

  it.each(["error_max_turns", "error_during_execution", "error_max_budget_usd", "error_max_structured_output_retries", "success"])("closes a failed result with a visible error: %s", subtype => {
    const events = new ClaudeEventParser("").parse({ type: "result", subtype, is_error: true, errors: ["Failed request"], uuid: "turn-error", session_id: "session-error", modelUsage: { model: { inputTokens: 3, outputTokens: 2, costUSD: 0.01 } } });
    expect(events.map(event => [event.type, event.error, event.content])).toEqual([
      ["session_started", undefined, undefined], ["usage", undefined, undefined], ["error", "Failed request", undefined], ["done", undefined, ""],
    ]);
  });

  it("uses whole-tree model totals instead of main-loop usage and captures session changes", () => {
    const parser = new ClaudeEventParser("");
    const events = [
      parser.parse({ type: "system", subtype: "init", session_id: "session-1" }),
      parser.parse({ type: "assistant", session_id: "session-1", message: { id: "m", content: [{ type: "text", text: "A" }], usage: { input_tokens: 4 } } }),
      parser.parse({ type: "assistant", session_id: "session-1", message: { id: "m", content: [{ type: "text", text: "B" }], usage: { input_tokens: 4 } } }),
      parser.parse({ type: "result", subtype: "success", session_id: "session-1", result: "B", uuid: "turn-1", usage: { input_tokens: 4, output_tokens: 2 }, modelUsage: { "actual-model": { inputTokens: 10, outputTokens: 8, costUSD: 0.12 } } }),
      parser.parse({ type: "system", subtype: "init", session_id: "session-2" }),
    ].flat();
    expect({ sessions: events.filter(event => event.type === "session_started").map(event => event.sessionId), usage: events.filter(event => event.type === "usage").map(event => event.usage) }).toEqual({
      sessions: ["session-1", "session-2"], usage: [{ inputTokens: 10, outputTokens: 8, cacheCreationInputTokens: 0, cacheReadInputTokens: 0, model: "actual-model", cumulative: { sessionId: "session-1", costUsd: 0.12 } }],
    });
  });
});
