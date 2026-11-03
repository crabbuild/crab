import { describe, expect, it } from "vitest";
import { ensureInitialPromptMessage } from "../electron/orchestrator-chat-history";

describe("orchestrator chat history", () => {
  it("creates an initial user message when a task has no persisted chat", () => {
    const messages = ensureInitialPromptMessage([], {
      id: "task-1",
      prompt: "Say hi",
      createdAt: 123,
    });

    expect(messages).toEqual([
      {
        id: "task-1-init",
        role: "user",
        content: "Say hi",
        timestamp: 123,
      },
    ]);
  });

  it("prepends the initial prompt when only agent messages were persisted", () => {
    const messages = ensureInitialPromptMessage(
      [
        {
          id: "agent-1",
          role: "agent",
          kind: "done",
          content: "Hi",
          timestamp: 456,
        },
      ],
      {
        id: "task-1",
        prompt: "Say hi",
        createdAt: 123,
      },
    );

    expect(messages.map((message) => message.role)).toEqual(["user", "agent"]);
    expect(messages[0].content).toBe("Say hi");
  });

  it("does not duplicate an existing user message", () => {
    const existing = [
      {
        id: "user-1",
        role: "user" as const,
        content: "Existing",
        timestamp: 100,
      },
    ];

    expect(ensureInitialPromptMessage(existing, {
      id: "task-1",
      prompt: "Say hi",
      createdAt: 123,
    })).toBe(existing);
  });
});
