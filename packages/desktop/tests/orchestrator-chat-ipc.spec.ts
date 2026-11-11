import { afterEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import type { Orchestrator, Task } from "../electron/orchestrator";
import { AgentSubmissionUncertainError, type AgentTurnOptions } from "../electron/orchestrator-adapters";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";

const owned: Array<{ db: ChatDatabase; directory: string }> = [];
afterEach(() => {
  vi.restoreAllMocks();
  for (const { db, directory } of owned.splice(0)) { db.close(); rmSync(directory, { recursive: true, force: true }); }
});
function fixture(deliver = true) {
  const directory = mkdtempSync(path.join(tmpdir(), "crab-chat-ipc-"));
  const db = new ChatDatabase(path.join(directory, "chat.sqlite"));
  owned.push({ db, directory });
  const task: Pick<Task, "id" | "prompt" | "createdAt" | "worktreePath" | "pendingInput"> = { id: "task", prompt: "Initial", createdAt: 1, worktreePath: directory };
  const owner = Object.assign(new EventEmitter(), {
    getTask: (id: string) => id === task.id ? task : undefined,
    iterate: vi.fn(async (taskId: string, content: string, _options?: AgentTurnOptions, id = "fixture-send"): Promise<void> => {
      owner.emit("orchestrator:submission", { taskId, id, content, state: "pending" });
      owner.emit("orchestrator:submission", { taskId, id, content, state: "accepted" });
    }),
    cancelTask: vi.fn(async (_id: string) => {}),
    connectCompletionJournal: vi.fn(),
    acknowledgeCompletion: vi.fn(),
    flushCompletions: vi.fn(),
  });
  const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
  const ipc = { handle: (channel: string, handler: Parameters<IpcMain["handle"]>[1]) => { handlers.set(channel, handler); } };
  const send = vi.fn();
  const window = { isDestroyed: () => false, webContents: { send } };
  const chat = registerChatIpc(ipc, owner as unknown as Orchestrator, db, () => deliver ? window : null);
  const invoke = async (channel: string, payload: unknown): Promise<unknown> => {
    const handler = handlers.get(channel);
    if (!handler) throw new Error(`Handler not registered: ${channel}`);
    return handler({} as IpcMainInvokeEvent, payload);
  };
  return { db, task, owner, chat, invoke, send };
}

describe("registered chat ownership", () => {
  it("loads background output through the registered IPC without a renderer", async () => {
    const { task, owner, invoke } = fixture(false);
    owner.emit("orchestrator:event", { taskId: task.id, event: { type: "text", content: "Background", itemId: "block", timestamp: 2 } });
    const result = await invoke("orchestrator:load-chat", { taskId: task.id }) as { success: boolean; messages: Array<{ content: string }> };
    expect({ success: result.success, messages: result.messages.map(message => message.content) }).toEqual({ success: true, messages: ["Initial", "Background"] });
  });

  it("does not persist a user message rejected by its provider submission", async () => {
    const { task, owner, invoke, chat } = fixture();
    owner.iterate.mockRejectedValue(new Error("Submission rejected"));
    const result = await invoke("orchestrator:iterate", { taskId: task.id, feedback: "Rejected", messageId: "user-send" });
    expect({ result, users: chat.snapshot(task.id).messages.filter(message => message.role === "user").map(message => message.content) }).toEqual({ result: { success: false, error: "Submission rejected" }, users: ["Initial"] });
  });

  it("exits native plan mode through the actual approve-plan IPC caller", async () => {
    const { task, owner, invoke, chat } = fixture();
    await invoke("orchestrator:approve-plan", { taskId: task.id });
    expect({ calls: owner.iterate.mock.calls, users: chat.snapshot(task.id).messages.filter(message => message.role === "user").map(message => message.content) }).toEqual({
      calls: [[task.id, "Plan approved. Proceed with implementation.", { mode: "instant" }, undefined]], users: ["Initial", "Plan approved. Proceed with implementation."],
    });
  });

  it("retains a definitively unsent setup failure for the renderer and explicit Retry", async () => {
    const { task, owner, invoke, chat } = fixture();
    owner.iterate.mockImplementationOnce(async () => {
      task.pendingInput = { id: "user-send", content: "Latest input", kind: "message", phase: "unsent", turnNumber: 1 };
      const submission = { taskId: task.id, id: "user-send", content: "Latest input" };
      owner.emit("orchestrator:submission", { ...submission, state: "pending" });
      owner.emit("orchestrator:submission", { ...submission, state: "unconfirmed", error: "Message was not sent. Use Retry." });
      throw new Error("Workspace setup failed");
    });
    const result = await invoke("orchestrator:iterate", { taskId: task.id, feedback: "Latest input", messageId: "user-send" });
    expect({ result, users: chat.snapshot(task.id).messages.filter(message => message.role === "user").map(message => message.content) }).toEqual({
      result: { success: false, unsent: true, error: "Message was not sent. Workspace setup failed. Use Retry to send it." }, users: ["Initial", "Latest input"],
    });
  });

  it("preserves a possibly accepted message and its delivery warning in durable order", async () => {
    const { task, owner, invoke, db } = fixture();
    owner.iterate.mockImplementationOnce(async () => {
      const submission = { taskId: task.id, id: "user-send", content: "Possibly accepted" };
      owner.emit("orchestrator:submission", { ...submission, state: "pending" });
      owner.emit("orchestrator:event", { taskId: task.id, event: { type: "text", content: "Provider acted", timestamp: 2 } });
      const error = new AgentSubmissionUncertainError(new Error("Transport closed"));
      owner.emit("orchestrator:submission", { ...submission, state: "unconfirmed", error: error.message });
      throw error;
    });
    const result = await invoke("orchestrator:iterate", { taskId: task.id, feedback: "Possibly accepted", messageId: "user-send" });
    const loaded = await invoke("orchestrator:load-chat", { taskId: task.id }) as { messages: Array<{ content: string }> };
    const saved = db.loadMessages(task.id).find(row => row.id === "user-send");
    expect({ result, content: loaded.messages.map(message => message.content), metadata: saved?.metadata && JSON.parse(saved.metadata).deliveryError }).toEqual({
      result: { success: false, uncertain: true, error: "Message delivery could not be confirmed: Transport closed" },
      content: ["Initial", "Possibly accepted", "Provider acted"], metadata: "Message delivery could not be confirmed: Transport closed",
    });
  });

  it("keeps the durable write and provider alive when renderer delivery throws", () => {
    const { task, owner, send, chat } = fixture();
    send.mockImplementation(() => { throw new Error("Renderer destroyed"); });
    owner.emit("orchestrator:event", { taskId: task.id, event: { type: "text", content: "Saved", timestamp: 2 } });
    expect({ cancelled: owner.cancelTask.mock.calls, contents: chat.snapshot(task.id).messages.map(message => message.content) }).toEqual({ cancelled: [], contents: ["Initial", "Saved"] });
  });

  it("reports a durable write failure, stops its provider once, and exposes the error to a later reader", async () => {
    const { task, owner, db, chat, invoke } = fixture();
    chat.snapshot(task.id);
    vi.spyOn(db, "updateMessages").mockImplementation(() => { throw new Error("Disk full"); });
    const errors: string[] = [];
    owner.on("orchestrator:error", (payload: { message: string }) => errors.push(payload.message));
    for (const content of ["one", "two"]) owner.emit("orchestrator:event", { taskId: task.id, event: { type: "text", content, timestamp: 2 } });
    const snapshot = await invoke("orchestrator:load-chat", { taskId: task.id }) as { persistenceError?: string };
    expect({ errors, cancelled: owner.cancelTask.mock.calls, laterError: snapshot.persistenceError }).toEqual({ errors: ["Could not save chat: Disk full"], cancelled: [[task.id]], laterError: "Could not save chat: Disk full" });
  });
});
