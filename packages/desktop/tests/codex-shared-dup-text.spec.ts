import { describe, expect, it } from "vitest";
import { parseCodexEvents, type CodexParseState } from "../electron/codex-shared";
import { applyAgentText } from "../electron/agent-chat-messages";
import { deriveTurns } from "../src/lib/turn-derivation";
import type { AgentEvent } from "../electron/orchestrator-adapters";
import type { ChatMessage } from "../electron/agent-chat-messages";

function stream(notifications: Array<{ method: string; params: Record<string, unknown> }>): AgentEvent[] {
  const state: CodexParseState = { agentMessages: new Map(), lastAgentMessage: "" };
  return notifications.flatMap(notification => parseCodexEvents(notification, state));
}
const delta = (itemId: string, text: string) => ({ method: "item/agentMessage/delta", params: { itemId, delta: text } });
const complete = (id: string, text: string) => ({ method: "item/completed", params: { item: { type: "agentMessage", id, text } } });
function rendered(events: AgentEvent[]) {
  return events.reduce<ChatMessage[]>((messages, event, index) => applyAgentText(messages, event, `message-${index}`), []).map(message => message.content);
}

describe("Codex assistant items", () => {
  it("keeps commentary, final output, repeated deltas and identical distinct messages", () => {
    const events = stream([
      delta("commentary", "Checking"), complete("commentary", "Checking"),
      delta("answer", "ha"), delta("answer", "ha"), complete("answer", "haha"),
      complete("another-answer", "haha"),
    ]);
    expect(rendered(events)).toEqual(["Checking", "haha", "haha"]);
  });

  it("replaces a partial block with its authoritative completion without losing whitespace", () => {
    const events = stream([delta("answer", "Partial"), complete("answer", " Corrected \n"), complete("answer", " Corrected \n")]);
    expect({ chat: rendered(events), turn: deriveTurns(events, [])[0].events.map(event => event.content) }).toEqual({
      chat: [" Corrected \n"], turn: [" Corrected \n"],
    });
  });

  it("keeps interleaved item streams separate in chat and turn rendering", () => {
    const events = stream([delta("a", "One "), delta("b", "Two "), delta("a", "A"), delta("b", "B")]);
    expect({ chat: rendered(events), turn: deriveTurns(events, [])[0].events.map(event => event.content) }).toEqual({
      chat: ["One A", "Two B"], turn: ["One A", "Two B"],
    });
  });
});
