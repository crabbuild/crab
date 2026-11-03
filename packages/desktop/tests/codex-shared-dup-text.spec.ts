/**
 * Regression test for the duplicated-text bug in parseCodexEvents.
 *
 * When item/agentMessage/delta events stream the full message text
 * incrementally, the subsequent item/completed envelope carrying the
 * same agentMessage.text must NOT emit a second text event. The
 * renderer concatenates every text event it receives, so a duplicate
 * emission doubles the visible message.
 *
 * Validates: Requirements CHAT9.7
 */

import { describe, it, expect } from "vitest";
import {
  parseCodexEvents,
  type CodexParseState,
} from "../electron/codex-shared";

function makeState(): CodexParseState {
  return { lastAgentMessage: "" };
}

describe("parseCodexEvents — duplicate text deduplication", () => {
  it("does not emit duplicate text when deltas have already streamed the full message", () => {
    const state = makeState();
    const fullMessage = "Hello world from codex.";

    // Simulate streaming deltas that accumulate to the full message.
    const deltas = ["Hello ", "world ", "from ", "codex."];
    const emittedTexts: string[] = [];

    for (const delta of deltas) {
      const events = parseCodexEvents(
        { type: "item.agentMessage.delta", delta },
        state,
      );
      for (const e of events) {
        if (e.type === "text") {
          emittedTexts.push(e.content as string);
        }
      }
    }

    // At this point, state.lastAgentMessage should equal the full message.
    expect(state.lastAgentMessage).toBe(fullMessage);

    // Now the item/completed envelope arrives with the same text.
    const completedEvents = parseCodexEvents(
      {
        type: "item.completed",
        item: { type: "agentMessage", text: fullMessage },
      },
      state,
    );

    // The completed envelope should NOT emit another text event because
    // the deltas already delivered the full content.
    for (const e of completedEvents) {
      if (e.type === "text") {
        emittedTexts.push(e.content as string);
      }
    }

    // The concatenation of all emitted text events should equal the
    // message exactly once — not twice.
    const rendered = emittedTexts.join("");
    expect(rendered).toBe(fullMessage);
  });

  it("emits the text event from item/completed when no deltas preceded it (replay/reconnect)", () => {
    const state = makeState();
    const fullMessage = "Hello world from codex.";

    // No deltas arrive — simulate a replay or reconnect scenario where
    // the renderer attaches after streaming has finished.
    const completedEvents = parseCodexEvents(
      {
        type: "item.completed",
        item: { type: "agentMessage", text: fullMessage },
      },
      state,
    );

    const emittedTexts: string[] = [];
    for (const e of completedEvents) {
      if (e.type === "text") {
        emittedTexts.push(e.content as string);
      }
    }

    // The completed envelope IS the only source of truth here, so it
    // must emit the text event.
    const rendered = emittedTexts.join("");
    expect(rendered).toBe(fullMessage);

    // State should be updated for any subsequent events.
    expect(state.lastAgentMessage).toBe(fullMessage);
  });

  it("also works with the app-server method-based wire format", () => {
    const state = makeState();
    const fullMessage = "Hello world from codex.";

    // Stream deltas via method-based format.
    const deltas = ["Hello ", "world ", "from ", "codex."];
    const emittedTexts: string[] = [];

    for (const delta of deltas) {
      const events = parseCodexEvents(
        { method: "item/agentMessage/delta", params: { delta } },
        state,
      );
      for (const e of events) {
        if (e.type === "text") {
          emittedTexts.push(e.content as string);
        }
      }
    }

    // item/completed via method format.
    const completedEvents = parseCodexEvents(
      {
        method: "item/completed",
        params: { item: { type: "agentMessage", text: fullMessage } },
      },
      state,
    );

    for (const e of completedEvents) {
      if (e.type === "text") {
        emittedTexts.push(e.content as string);
      }
    }

    const rendered = emittedTexts.join("");
    expect(rendered).toBe(fullMessage);
  });

  it("emits text from item/completed when deltas delivered only a partial message", () => {
    const state = makeState();
    const fullMessage = "Hello world from codex.";

    // Only partial deltas arrive (simulating a dropped connection mid-stream).
    const partialDeltas = ["Hello ", "world "];
    for (const delta of partialDeltas) {
      parseCodexEvents({ type: "item.agentMessage.delta", delta }, state);
    }

    expect(state.lastAgentMessage).toBe("Hello world ");

    // The completed envelope has the full message which differs from
    // the accumulated state — it should emit.
    const completedEvents = parseCodexEvents(
      {
        type: "item.completed",
        item: { type: "agentMessage", text: fullMessage },
      },
      state,
    );

    const emittedTexts: string[] = [];
    for (const e of completedEvents) {
      if (e.type === "text") {
        emittedTexts.push(e.content as string);
      }
    }

    // The full message is emitted because it doesn't match the partial accumulation.
    expect(emittedTexts.join("")).toBe(fullMessage);
    expect(state.lastAgentMessage).toBe(fullMessage);
  });

  it("suppresses late item/completed replay after turn completion", () => {
    const state = makeState();
    const fullMessage = "Hello world from codex.";

    for (const delta of ["Hello ", "world ", "from ", "codex."]) {
      parseCodexEvents({ type: "item.agentMessage.delta", delta }, state);
    }

    const doneEvents = parseCodexEvents({ type: "turn.completed" }, state);
    expect(doneEvents).toHaveLength(1);
    expect(doneEvents[0]).toMatchObject({ type: "done", content: fullMessage });
    expect(state.lastAgentMessage).toBe("");
    expect(state.lastCompletedAgentMessage).toBe(fullMessage);

    const lateEvents = parseCodexEvents(
      {
        type: "item.completed",
        item: { type: "agentMessage", text: fullMessage },
      },
      state,
    );

    expect(lateEvents).toEqual([]);
    expect(state.lastAgentMessage).toBe("");
  });
});
