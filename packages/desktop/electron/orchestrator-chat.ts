import { randomUUID } from "node:crypto";
import { ChatDatabase, type ChatMessageRow } from "./orchestrator-chat-db";
import { applyAgentEvent, type ChatMessage, type ChatUpdate } from "./agent-chat-messages";
import { AgentSubmissionUncertainError, pathVariants, type AgentEvent } from "./orchestrator-adapters";
import type { Task } from "./orchestrator";
import type { Orchestrator } from "./orchestrator";
import type { TaskSubmission } from "./orchestrator";
import type { IpcMain } from "electron";
import type { AgentTurnOptions } from "./orchestrator-adapters";

type ChatTask = Pick<Task, "id" | "prompt" | "createdAt" | "worktreePath">;
type Transcript = { messages: ChatMessage[]; revision: number; sequence: number };

/** Owns durable transcripts independently of renderer mounting and selection. */
export class ChatJournal {
  private transcripts = new Map<string, Transcript>();

  constructor(private readonly db: ChatDatabase, private readonly task: (id: string) => ChatTask | undefined) {}

  snapshot(taskId: string): { messages: ChatMessage[]; revision: number } {
    const state = this.load(taskId);
    return { messages: state.messages, revision: state.revision };
  }

  event(taskId: string, event: AgentEvent, id?: string): ChatUpdate | undefined {
    const state = this.load(taskId);
    if (id && state.messages.some(message => message.id === id)) return undefined;
    const workspace = this.task(taskId)?.worktreePath;
    const paths = workspace ? pathVariants(workspace) : [];
    const messages = applyAgentEvent(state.messages, event, id ?? randomUUID(), paths);
    return this.commit(taskId, messages);
  }

  userMessage(taskId: string, content: string, id: string = randomUUID()): ChatMessage {
    const state = this.load(taskId);
    // Reserve ordering before submission. A fast provider may finish before
    // acknowledging the send; a definitive rejection removes the pending row.
    return { id, role: "user", content, timestamp: Date.now(), sequence: ++state.sequence };
  }

  append(taskId: string, message: ChatMessage): ChatUpdate | undefined {
    const state = this.load(taskId);
    return this.commit(taskId, [...state.messages, message]);
  }

  submission(submission: TaskSubmission): ChatUpdate | undefined {
    const { taskId, id, content, state } = submission;
    if (state === "pending") return this.append(taskId, { ...this.userMessage(taskId, content, id), pendingDelivery: true });
    const messages = this.load(taskId).messages;
    if (state === "rejected") return this.commit(taskId, messages.filter(message => message.id !== id));
    return this.commit(taskId, messages.map(message => {
      if (message.id !== id) return message;
      // The journal may have saved acceptance before task metadata failed.
      // Restart's unknown delivery cannot revoke that durable acknowledgement.
      if (state === "unconfirmed" && !message.pendingDelivery && !message.deliveryError) return message;
      return { ...message, pendingDelivery: undefined, deliveryError: state === "unconfirmed" ? submission.error : undefined };
    }));
  }

  note(taskId: string, content: string, kind: "text" | "error" = "text"): ChatUpdate | undefined {
    const message: ChatMessage = { id: randomUUID(), role: "agent", kind, content, timestamp: Date.now() };
    return this.append(taskId, message);
  }

  delete(taskId: string): void {
    this.db.deleteTaskMessages(taskId);
    this.transcripts.delete(taskId);
  }

  private commit(taskId: string, messages: ChatMessage[]): ChatUpdate | undefined {
    const state = this.load(taskId);
    const previous = new Map(state.messages.map(message => [message.id, message]));
    const upsert = messages.filter(message => previous.get(message.id) !== message)
      .map(message => message.sequence === undefined ? { ...message, sequence: ++state.sequence } : message);
    const retained = new Set(messages.map(message => message.id));
    const deleted = state.messages.filter(message => !retained.has(message.id)).map(message => message.id);
    if (!upsert.length && !deleted.length) return undefined;
    const rows = upsert.map(message => this.row(taskId, message));
    this.db.updateMessages(taskId, rows, deleted);
    const updates = new Map(upsert.map(message => [message.id, message]));
    state.messages = messages.map(message => updates.get(message.id) ?? message)
      .sort((a, b) => (a.sequence ?? 0) - (b.sequence ?? 0));
    return { taskId, revision: ++state.revision, upsert, ...(deleted.length ? { deleted } : {}) };
  }

  private row(taskId: string, message: ChatMessage): ChatMessageRow {
    const { id, role, kind, content, events, timestamp, ...metadata } = message;
    return { id, task_id: taskId, role, kind: kind ?? null, content, events: events ? JSON.stringify(events) : null,
      timestamp, metadata: JSON.stringify(metadata) };
  }

  private load(taskId: string): Transcript {
    const cached = this.transcripts.get(taskId);
    if (cached) return cached;
    const rows = this.db.loadMessages(taskId);
    const messages: ChatMessage[] = rows.map(row => ({
      id: row.id, role: row.role, kind: (row.kind ?? undefined) as ChatMessage["kind"], content: row.content,
      timestamp: row.timestamp, events: row.events ? JSON.parse(row.events) : undefined,
      ...(row.metadata ? JSON.parse(row.metadata) : {}),
    }));
    const task = this.task(taskId);
    if (task && !messages.some(message => message.role === "user")) {
      messages.unshift({ id: `${taskId}-init`, role: "user", content: task.prompt, timestamp: task.createdAt });
    }
    // These records predate this connection owner. Old prompts must not look
    // answerable after restart; current connections populate the cache first.
    const restored = messages.map((message, index) => {
      const events = message.events?.map(tool => {
        const input = tool.input as { requestId?: string; resolved?: boolean } | undefined;
        return (tool.tool === "AskUserQuestion" || tool.tool === "CodexApproval") && input?.requestId && !input.resolved
          ? { ...tool, input: { ...input, resolved: true, resolutionReason: "Connection closed" } } : tool;
      });
      if (!message.pendingDelivery && message.sequence !== undefined && !events?.some((tool, position) => tool !== message.events?.[position])) return message;
      return { ...message, sequence: message.sequence ?? index + 1, events,
        ...(message.pendingDelivery ? { pendingDelivery: undefined, deliveryError: "Connection closed before message delivery was confirmed" } : {}),
      };
    });
    const changed = restored.filter((message, index) => message !== messages[index]);
    this.db.updateMessages(taskId, changed.map(message => this.row(taskId, message)));
    restored.sort((a, b) => (a.sequence ?? 0) - (b.sequence ?? 0));
    const sequence = restored.reduce((maximum, message) => Math.max(maximum, message.sequence ?? 0), 0);
    const state = { messages: restored, revision: 0, sequence };
    this.transcripts.set(taskId, state);
    return state;
  }
}

interface ChatWindow {
  isDestroyed(): boolean;
  webContents: { send(channel: string, payload: unknown): void };
}

/** Registers the transcript owner and its submission/read IPC boundary. */
export function registerChatIpc(ipc: Pick<IpcMain, "handle">, orchestrator: Orchestrator, db: ChatDatabase, window: () => ChatWindow | null): ChatJournal {
  const chat = new ChatJournal(db, id => orchestrator.getTask(id));
  const failures = new Map<string, string>();
  const relay = (channel: string, payload: unknown) => {
    const target = window();
    if (!target || target.isDestroyed()) return;
    // The durable commit precedes delivery. Losing a renderer must not stop
    // its provider or turn a successful write into a persistence failure.
    try { target.webContents.send(channel, payload); } catch { /* The next view loads the committed snapshot. */ }
  };
  const record = (taskId: string, operation: () => ChatUpdate | undefined, completionId?: string) => {
    let update: ChatUpdate | undefined;
    try {
      // Buffered frames can arrive while a failed journal write is stopping
      // the CLI. Preserve terminal ordering before accepting any later row.
      orchestrator.flushCompletions(taskId, completionId);
      update = operation();
    } catch (error: unknown) {
      if (!failures.has(taskId)) {
        const message = `Could not save chat: ${error instanceof Error ? error.message : String(error)}`;
        failures.set(taskId, message);
        orchestrator.emit("orchestrator:error", { taskId, message });
        void orchestrator.cancelTask(taskId).catch(() => {});
      }
      return false;
    }
    if (update) relay("orchestrator:chat-update", update);
    return true;
  };
  orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent; completionId?: string }) => {
    const completionId = payload.completionId;
    const saved = record(payload.taskId, () => chat.event(payload.taskId, payload.event, completionId), completionId);
    if (saved && completionId) record(payload.taskId, () => {
      // The committed patch must be delivered even if the separate receipt
      // cannot be cleared. Replay deduplicates that row by its stable ID.
      orchestrator.acknowledgeCompletion(payload.taskId, completionId);
      return undefined;
    }, completionId);
    relay("orchestrator:event", payload);
  });
  orchestrator.on("orchestrator:task-status", (payload: { taskId: string; status: string }) => {
    if (payload.status === "running") failures.delete(payload.taskId);
  });
  orchestrator.on("orchestrator:chat-user", (payload: { taskId: string; content: string }) => {
    // Workflow invocations are application commands on the source task;
    // provider follow-ups, including workflow node feedback, use submissions.
    record(payload.taskId, () => chat.append(payload.taskId, chat.userMessage(payload.taskId, payload.content)));
  });
  orchestrator.on("orchestrator:submission", (submission: TaskSubmission) => {
    const saved = record(submission.taskId, () => chat.submission(submission));
    if (!saved) throw new Error(failures.get(submission.taskId));
  });
  const submit = async (taskId: string, content: string, options?: AgentTurnOptions, id?: string) => {
    try {
      await orchestrator.iterate(taskId, content, options, id);
      return { success: true };
    } catch (error: unknown) {
      if (error instanceof AgentSubmissionUncertainError) return { success: false, uncertain: true, error: error.message };
      const input = orchestrator.getTask(taskId)?.pendingInput;
      const unsent = input?.phase === "unsent" && !input.outcome && input.content === content && (id === undefined || input.id === id);
      const message = error instanceof Error ? error.message : String(error);
      return { success: false, ...(unsent ? { unsent: true } : {}), error: unsent ? `Message was not sent. ${message}. Use Retry to send it.` : message };
    }
  };
  ipc.handle("orchestrator:iterate", (_event, payload: { taskId: string; feedback: string; options?: AgentTurnOptions; messageId?: string }) => submit(payload.taskId, payload.feedback, payload.options, payload.messageId));
  ipc.handle("orchestrator:approve-plan", (_event, payload: { taskId: string }) => submit(payload.taskId, "Plan approved. Proceed with implementation.", { mode: "instant" }));
  ipc.handle("orchestrator:append-chat-note", (_event, payload: { taskId: string; content: string; kind?: "text" | "error" }) => {
    try {
      if (!orchestrator.getTask(payload.taskId)) throw new Error(`Task not found: ${payload.taskId}`);
      orchestrator.flushCompletions(payload.taskId);
      const update = chat.note(payload.taskId, payload.content, payload.kind);
      if (update) relay("orchestrator:chat-update", update);
      return { success: true };
    } catch (error: unknown) {
      return { success: false, error: error instanceof Error ? error.message : String(error) };
    }
  });
  ipc.handle("orchestrator:load-chat", (_event, payload: { taskId: string }) => {
    try {
      if (!orchestrator.getTask(payload.taskId)) throw new Error(`Task not found: ${payload.taskId}`);
      return { success: true, ...chat.snapshot(payload.taskId), persistenceError: failures.get(payload.taskId) };
    } catch (error: unknown) {
      return { success: false, error: error instanceof Error ? error.message : String(error) };
    }
  });
  orchestrator.connectCompletionJournal();
  return chat;
}
