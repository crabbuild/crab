import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { ChatTranscript } from "../../src/components/orchestrator/ChatTranscript";
import { applyAgentEvent, type ChatMessage } from "../../electron/agent-chat-messages";
import type { AgentEvent } from "../../electron/orchestrator-adapters";
import { Button } from "../../src/components/ui/button";
import "../../src/index.css";

declare global {
  interface Window {
    teamFixture: { calls: Array<{ id: string; text: string }>; fail: boolean; replace: (messages: ChatMessage[]) => void };
  }
}
window.crab = { orchestrator: {
  getTaskUIState: async () => ({ success: true, state: null }), setTaskUIState: async () => ({ success: true }),
  iterate: async (id: string, text: string) => {
    window.teamFixture.calls.push({ id, text });
    if (window.teamFixture.fail) throw new Error("Agent connection lost. Retry after reconnecting.");
    return { success: true };
  }, cancelTask: async () => ({ success: true }),
} } as unknown as Window["crab"];
const longOutput = "The lease belongs to the ref owner.\n\n".repeat(180) + "FULL_TRANSCRIPT_END";
const events: AgentEvent[] = [
  { type: "turn_started", timestamp: 1, turnId: "lead-turn" },
  { type: "text", timestamp: 2, content: "I’m tracing ownership with a Codex reviewer and a Claude investigator." },
  { type: "subagent", timestamp: 3, subagent: { id: "reviewer", taskId: "reviewer", name: "Ownership review", provider: "codex", status: "completed", prompt: "Review the ref lock lifecycle. Follow acquisition through push and release; report any unhandled exit paths.",
    transcript: [
      { type: "turn_started", timestamp: 3, turnId: "review" },
      { type: "tool_use", timestamp: 4, itemId: "read", tool: "Read", input: { file_path: "crab/src/cmd/push.rs" } },
      { type: "tool_result", timestamp: 5, itemId: "read", input: { final: true }, output: "Lease ownership is checked before ref publication." },
      { type: "text", timestamp: 6, itemId: "result", content: longOutput },
      { type: "command_output", timestamp: 6, commandSessionId: "verification", content: "COMMAND_SESSION_" },
      { type: "command_output", timestamp: 6, commandSessionId: "verification", content: "BYTES_END" },
      { type: "done", timestamp: 7, outcome: "completed" },
    ] } },
  { type: "subagent", timestamp: 8, subagent: { id: "investigator", taskId: "investigator", name: "Failure paths", provider: "claude-code", status: "running", prompt: "Check cancellation while a push is in progress.",
    event: { type: "text", timestamp: 8, content: "The cancellation handler joins the current owner before releasing its lease." } } },
  { type: "subagent", timestamp: 9, subagent: { id: "investigator", event: { type: "subagent", timestamp: 9,
    subagent: { id: "nested", name: "Regression check", provider: "claude-code", status: "queued", prompt: "Check adjacent concurrency tests." } } } },
  { type: "subagent", timestamp: 9, subagent: { id: "investigator", event: { type: "subagent", timestamp: 9,
    subagent: { id: "nested", event: { type: "subagent", timestamp: 9, subagent: { id: "grandchild", name: "Lock contention", provider: "claude-code", status: "queued", prompt: "Check lock contention coverage." } } } } } },
  { type: "agent_message", timestamp: 10, agentMessage: { from: "reviewer", to: "investigator", content: "Please verify that cancellation waits for the lock owner before cleanup.", delivery: "accepted" } },
];
const initial = events.reduce((messages, event, index) => applyAgentEvent(messages, event, String(index)), [] as ChatMessage[]);
function Fixture() {
  const [messages, setMessages] = useState(initial);
  window.teamFixture = { calls: window.teamFixture?.calls ?? [], fail: window.teamFixture?.fail ?? false, replace: setMessages };
  return <main className="mx-auto max-w-4xl px-6 py-8"><header className="mb-8 flex items-center justify-between"><h1 className="text-xl font-medium">Ref lock lifecycle</h1>
    <Button onClick={() => document.documentElement.classList.toggle("theme-light")}>Toggle theme</Button></header>
    <ChatTranscript taskId="lead" messages={messages} /></main>;
}
createRoot(document.getElementById("root")!).render(<Fixture />);
