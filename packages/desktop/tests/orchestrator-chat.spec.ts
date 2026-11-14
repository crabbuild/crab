import { afterEach, describe, expect, it } from "vitest";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { ChatJournal } from "../electron/orchestrator-chat";
import { applyChatUpdate, type ChatMessage } from "../electron/agent-chat-messages";

const owned: Array<{ db: ChatDatabase; directory: string }> = [];
afterEach(() => {
  for (const { db, directory } of owned.splice(0)) { db.close(); rmSync(directory, { recursive: true, force: true }); }
});
function journal() {
  const directory = mkdtempSync(path.join(tmpdir(), "crab-chat-journal-"));
  const db = new ChatDatabase(path.join(directory, "chats.sqlite"));
  owned.push({ db, directory });
  const task = { id: "task", prompt: "Initial prompt", createdAt: 1, worktreePath: directory };
  return { db, task, chat: new ChatJournal(db, id => id === task.id ? task : undefined) };
}

describe("main-process transcript ownership", () => {
  it("restores complete sub-agent output and command bytes under its stable identity", () => {
    const { db, task, chat } = journal();
    const content = "Detailed finding\n".repeat(600) + "COMPLETE_RESULT_END";
    chat.event(task.id, { type: "subagent", timestamp: 2, subagent: { id: "tool", name: "Reviewer", prompt: "Review ownership" } });
    chat.event(task.id, { type: "subagent", timestamp: 3, subagent: { id: "stable", toolCallId: "tool", status: "running" } });
    chat.event(task.id, { type: "subagent", timestamp: 4, subagent: { id: "stable", event: { type: "text", timestamp: 4, content } } });
    chat.event(task.id, { type: "subagent", timestamp: 5, subagent: { id: "stable", event: { type: "command_output", timestamp: 5, commandSessionId: "command", content: "FULL_COMMAND_BYTES" } } });
    const restored = new ChatJournal(db, () => task).snapshot(task.id).messages.filter(message => message.subagent);
    expect(restored.map(message => ({ id: message.subagent?.id, name: message.subagent?.name, prompt: message.subagent?.prompt,
      output: message.messages?.map(entry => entry.content || entry.providerEvent?.content) })))
      .toEqual([{ id: "stable", name: "Reviewer", prompt: "Review ownership", output: [content, "FULL_COMMAND_BYTES"] }]);
  });

  it("removes only a definitively rejected send from durable history, search, and renderer patches", () => {
    const { db, task, chat } = journal();
    const submission = { taskId: task.id, id: "send", content: "rejectedword" };
    chat.submission({ ...submission, state: "pending" });
    const before = chat.snapshot(task.id).messages;
    const patch = chat.submission({ ...submission, state: "rejected" });
    const replica = patch ? applyChatUpdate(before, patch) : before;
    expect({ durable: db.loadMessages(task.id).map(message => message.content), replica: replica.map(message => message.content), search: db.search("rejectedword") }).toEqual({ durable: ["Initial prompt"], replica: ["Initial prompt"], search: [] });
  });

  it("restores an unfinished send as unconfirmed after connection loss", () => {
    const { db, task, chat } = journal();
    chat.submission({ taskId: task.id, id: "send", content: "Pending intent", state: "pending" });
    const restored = new ChatJournal(db, () => task).snapshot(task.id).messages.find(message => message.id === "send");
    expect({ content: restored?.content, pending: restored?.pendingDelivery, error: restored?.deliveryError }).toEqual({ content: "Pending intent", pending: undefined, error: "Connection closed before message delivery was confirmed" });
  });

  it("preserves a journaled acceptance when restart recovers unknown task metadata", () => {
    const { db, task, chat } = journal();
    const submission = { taskId: task.id, id: "accepted-input", content: "Accepted input" };
    chat.submission({ ...submission, state: "pending" });
    chat.submission({ ...submission, state: "accepted" });
    const restored = new ChatJournal(db, () => task);
    restored.submission({ ...submission, state: "unconfirmed", error: "Task metadata was not saved" });
    expect(restored.snapshot(task.id).messages.find(message => message.id === submission.id)?.deliveryError).toBeUndefined();
  });

  it("restores tools, output, errors, and closed pending requests without any renderer save", () => {
    const { db, task, chat } = journal();
    chat.event(task.id, { type: "turn_started", turnId: "turn", timestamp: 2 });
    chat.event(task.id, { type: "tool_use", tool: "Read", input: { file_path: "file.txt" }, itemId: "read", timestamp: 3 });
    chat.event(task.id, { type: "approval_request", timestamp: 4, approval: { requestId: "approval", kind: "command", summary: "Run command", blocking: true } });
    chat.event(task.id, { type: "tool_result", itemId: "read", output: "File contents", input: { final: true }, timestamp: 5 });
    chat.event(task.id, { type: "error", error: "Provider failed", timestamp: 6 });
    chat.event(task.id, { type: "done", outcome: "failed", timestamp: 7 });
    const restored = new ChatJournal(db, () => task).snapshot(task.id).messages;
    expect({
      prompt: restored[0].content,
      tool: restored.flatMap(message => message.events ?? []).find(tool => tool.itemId === "read")?.detail,
      approval: restored.flatMap(message => message.events ?? []).find(tool => tool.tool === "CodexApproval")?.input,
      error: restored.find(message => message.kind === "error")?.content,
      outcome: restored.find(message => message.kind === "done")?.outcome,
    }).toEqual({ prompt: "Initial prompt", tool: "File contents", approval: {
      requestId: "approval", kind: "command", summary: "Run command", blocking: true, resolved: true, resolutionReason: "Connection closed",
    }, error: "Provider failed", outcome: "failed" });
  });

  it("retains repeated output and reuses block ids only within their own turn", () => {
    const { task, chat } = journal();
    for (const turnId of ["first", "second"]) {
      chat.event(task.id, { type: "turn_started", turnId, timestamp: 2 });
      chat.event(task.id, { type: "text", itemId: "block", content: "ha", timestamp: 3 });
      chat.event(task.id, { type: "text", itemId: "block", content: "ha", timestamp: 4 });
      chat.event(task.id, { type: "done", outcome: "completed", content: "haha", timestamp: 5 });
    }
    expect(chat.snapshot(task.id).messages.filter(message => message.kind === "text").map(message => [message.turnId, message.content])).toEqual([
      ["first", "haha"], ["second", "haha"],
    ]);
  });

  it("places an acknowledged send before a provider that finishes before acknowledging it", () => {
    const { task, chat } = journal();
    const message = chat.userMessage(task.id, "Follow up");
    chat.event(task.id, { type: "turn_started", turnId: "turn", timestamp: 2 });
    chat.event(task.id, { type: "text", itemId: "answer", content: "Answer", timestamp: 3 });
    chat.event(task.id, { type: "done", outcome: "completed", timestamp: 4 });
    chat.append(task.id, message);
    expect(chat.snapshot(task.id).messages.map(message => message.content || message.kind)).toEqual([
      "Initial prompt", "Follow up", "turn_started", "Answer", "done",
    ]);
  });

  it("keeps active requests answerable on reselect and persists provider resolution", () => {
    const { task, chat } = journal();
    chat.event(task.id, { type: "input_request", inputRequest: { requestId: "question", questions: [{ id: "choice", question: "Choose?" }] }, timestamp: 2 });
    const active = chat.snapshot(task.id).messages.flatMap(message => message.events ?? [])[0].input;
    chat.event(task.id, { type: "request_resolved", requestId: "question", timestamp: 3 });
    const closed = chat.snapshot(task.id).messages.flatMap(message => message.events ?? [])[0].input;
    expect({ active, closed }).toEqual({
      active: { requestId: "question", questions: [{ id: "choice", question: "Choose?" }] },
      closed: { requestId: "question", questions: [{ id: "choice", question: "Choose?" }], resolved: true },
    });
  });

  it("applies a late history snapshot followed by newer patches without losing tools or repeating text", () => {
    const { task, chat } = journal();
    chat.event(task.id, { type: "turn_started", turnId: "turn", timestamp: 2 });
    const snapshot = chat.snapshot(task.id);
    const first = chat.event(task.id, { type: "text", itemId: "block", content: "ha", timestamp: 3 });
    const second = chat.event(task.id, { type: "text", itemId: "block", content: "ha", timestamp: 4 });
    let restored: ChatMessage[] = snapshot.messages;
    for (const patch of [first, second]) if (patch && patch.revision > snapshot.revision) restored = applyChatUpdate(restored, patch);
    expect(restored.filter(message => message.kind === "text").map(message => message.content)).toEqual(["haha"]);
  });
});
