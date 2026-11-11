import type { AgentEvent } from "./orchestrator-adapters";

export interface ChatMessage {
  id: string;
  role: "user" | "agent";
  content: string;
  timestamp: number;
  /** Provider text-block identity, preserved across streamed updates. */
  itemId?: string;
  turnId?: string;
  /** Reserved before submission so a fast reply cannot precede its user message. */
  sequence?: number;
  deliveryError?: string;
  pendingDelivery?: boolean;
  outcome?: AgentEvent["outcome"];
  toolResult?: AgentEvent;
  providerEvent?: AgentEvent;
  /** Semantic type for agent messages — prevents merging different kinds. */
  kind?: "text" | "thinking" | "error" | "tool" | "done" | "turn_started";
  /** Inline tool-use events rendered within agent messages. */
  events?: ToolEvent[];
}

export interface ToolEvent {
  type: "file_read" | "file_edit" | "command" | "search" | "web" | "task" | "generic" | "thinking";
  /** The raw tool name from the adapter */
  tool?: string;
  summary: string;
  /** Full tool input object (for expanded view) */
  input?: unknown;
  /** Previous input state (for TodoWrite delta computation) */
  previousInput?: unknown;
  detail?: string;
  /** Codex item identifier — used by the AgentTasksView reducer to route
   *  streaming output deltas back to the card that owns this item. Set
   *  on tool_use events for items that emit subsequent output (Bash,
   *  Write, MCP, plan). */
  itemId?: string;
}

export interface Checkpoint {
  turnNumber: number;
  timestamp: number;
}

export interface ChatUpdate {
  taskId: string;
  revision: number;
  upsert: ChatMessage[];
  deleted?: string[];
}

export function applyChatUpdate(messages: ChatMessage[], update: ChatUpdate): ChatMessage[] {
  const byId = new Map(messages.map(message => [message.id, message]));
  for (const id of update.deleted ?? []) byId.delete(id);
  for (const message of update.upsert) byId.set(message.id, message);
  return [...byId.values()].sort((a, b) => (a.sequence ?? 0) - (b.sequence ?? 0));
}

/** Apply provider text blocks without guessing duplicates from their contents. */
export function applyAgentText(messages: ChatMessage[], event: AgentEvent, id: string): ChatMessage[] {
  const kind = event.type === "thinking" ? "thinking" : "text";
  const index = event.itemId
    ? messages.findIndex(message => message.role === "agent" && message.kind === kind && message.itemId === event.itemId && message.turnId === event.turnId)
    : messages.length - 1;
  const previous = messages[index];
  if (previous?.role === "agent" && previous.kind === kind && previous.itemId === event.itemId && previous.turnId === event.turnId) {
    const updated = [...messages];
    updated[index] = { ...previous, content: event.textMode === "replace" ? event.content ?? "" : previous.content + (event.content ?? ""), timestamp: event.timestamp };
    return updated;
  }
  return [...messages, { id, role: "agent", kind, itemId: event.itemId, turnId: event.turnId, content: event.content ?? "", timestamp: event.timestamp }];
}

function object(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}

function last<T>(values: readonly T[], matches: (value: T) => boolean): T | undefined {
  for (let index = values.length - 1; index >= 0; index--) if (matches(values[index])) return values[index];
  return undefined;
}

function toolType(tool: string): ToolEvent["type"] {
  const name = tool.toLowerCase();
  if (["bash", "terminal", "powershell"].includes(name) || name.includes("command")) return "command";
  if (["write", "edit", "filedelete"].includes(name) || /edit|write|delete/.test(name)) return "file_edit";
  if (["read", "imageview", "image_view"].includes(name) || name.includes("read")) return "file_read";
  if (["grep", "glob"].includes(name) || /search|find/.test(name)) return "search";
  if (/web|fetch/.test(name)) return "web";
  if (name === "todowrite" || /task|plan/.test(name)) return "task";
  return "generic";
}

/** Projects provider events into the one persisted, rendered transcript. */
export function applyAgentEvent(messages: ChatMessage[], raw: AgentEvent, id: string, workspacePaths: string[] = []): ChatMessage[] {
  const start = last(messages, message => message.kind === "turn_started");
  const event = { ...raw, turnId: raw.turnId ?? start?.turnId };
  const message = (fields: Partial<ChatMessage>): ChatMessage => ({
    id, role: "agent", content: "", timestamp: raw.timestamp, turnId: event.turnId, ...fields,
  });
  const replace = (index: number, value: ChatMessage): ChatMessage[] => messages.map((entry, position) => position === index ? value : entry);
  const relative = (value: string): string => workspacePaths.reduce((text, prefix) => text.replaceAll(prefix + "/", "").replaceAll(prefix + "\\", ""), value);
  const replaceTool = (index: number, toolIndex: number, value: ToolEvent, toolResult?: AgentEvent): ChatMessage[] => {
    const previous = messages[index];
    const events = previous.events?.map((entry, position) => position === toolIndex ? value : entry);
    return replace(index, { ...previous, events, ...(toolResult ? { toolResult } : {}) });
  };
  if (raw.type === "turn_started") return [...messages, message({ kind: "turn_started", turnId: raw.turnId ?? id })];
  if (raw.type === "command_output" || raw.type === "command_exited" || raw.type === "filesystem_change") {
    return [...messages, message({ kind: "tool", providerEvent: raw })];
  }
  const boundary = messages.reduce((index, entry, position) => entry.kind === "done" || entry.kind === "turn_started" || (!event.turnId && entry.role === "user") ? position : index, -1);
  if (raw.type === "text" || raw.type === "thinking") {
    const prefix = messages.slice(0, boundary + 1);
    return [...prefix, ...applyAgentText(messages.slice(boundary + 1), event, id)];
  }
  if (raw.type === "error") return [...messages, message({ kind: "error", content: raw.error ?? raw.content ?? "Provider error" })];
  if (raw.type === "done") {
    const hasText = messages.slice(boundary + 1).some(entry => entry.kind === "text");
    return [...messages, message({ kind: "done", outcome: raw.outcome, content: hasText ? "" : raw.content ?? "" })];
  }
  if (raw.type === "request_resolved") {
    return messages.map(entry => {
      const events = entry.events?.map(tool => object(tool.input).requestId === raw.requestId
        ? { ...tool, input: { ...object(tool.input), resolved: true } } : tool);
      return events?.some((tool, index) => tool !== entry.events?.[index]) ? { ...entry, events } : entry;
    });
  }
  if (raw.type === "tool_result") {
    let index = -1;
    let toolIndex = -1;
    for (let position = messages.length - 1; position >= 0 && index === -1; position--) {
      if (raw.turnId && messages[position].turnId !== raw.turnId) continue;
      const tools = messages[position].events;
      if (!tools?.length) continue;
      const match = raw.itemId ? tools.findIndex(tool => tool.itemId === raw.itemId) : tools.length - 1;
      if (match !== -1) { index = position; toolIndex = match; }
    }
    // An identified result must never land on a different concurrent tool.
    if (index === -1 || raw.output === undefined || raw.output === null) return messages;
    const output = typeof raw.output === "string" ? raw.output : Array.isArray(raw.output)
      ? raw.output.map(block => typeof block === "string" ? block : String(object(block).text ?? "")).join("\n")
      : JSON.stringify(raw.output);
    const tool = messages[index].events![toolIndex];
    const detail = object(raw.input).final === true ? output : (tool.detail ?? "") + output;
    const result = { ...raw, output: detail, input: { ...object(raw.input), final: true } };
    return replaceTool(index, toolIndex, { ...tool, detail }, result);
  }
  let tool: ToolEvent;
  if (raw.type === "approval_request" && raw.approval) {
    tool = { type: "generic", tool: "CodexApproval", summary: raw.approval.summary, input: raw.approval, itemId: raw.itemId };
  } else if (raw.type === "input_request" && raw.inputRequest) {
    const input = raw.inputRequest;
    const summary = input.kind === "elicitation" ? `${input.serverName}: ${input.message}` : "Agent questions";
    tool = { type: "generic", tool: "AskUserQuestion", summary, input, itemId: raw.itemId };
  } else if (["tool_use", "file_edit", "file_read", "command", "command_run", "search", "web", "task", "generic"].includes(raw.type)) {
    const name = raw.tool ?? raw.type;
    const input = { ...object(raw.input) };
    for (const field of ["file_path", "path", "directory", "old_path", "new_path", "command"]) {
      if (typeof input[field] === "string") input[field] = relative(input[field]);
    }
    const path = relative(raw.path ?? raw.command ?? "");
    const type = raw.type === "tool_use" ? toolType(name)
      : raw.type === "command_run" ? "command" : raw.type as ToolEvent["type"];
    const cleanedInput = raw.input !== null && typeof raw.input === "object" && !Array.isArray(raw.input) ? input : raw.input;
    tool = { type, tool: name, summary: path ? `${name}: ${path}` : name, input: cleanedInput,
      itemId: raw.itemId, detail: raw.diff ?? raw.stdout };
    if (name === "TodoWrite") tool.previousInput = last(messages.flatMap(entry => entry.events ?? []), entry => entry.tool === name)?.input;
  } else return messages;
  if (tool.itemId) {
    for (let index = messages.length - 1; index >= 0; index--) {
      const previous = messages[index];
      if (previous.turnId !== event.turnId) continue;
      const toolIndex = previous.events?.findIndex(entry => entry.itemId === tool.itemId && (entry.tool === tool.tool || raw.type === "input_request")) ?? -1;
      if (toolIndex === -1) continue;
      const old = previous.events![toolIndex];
      if (raw.type !== "input_request") {
        const input = { ...object(old.input), ...object(tool.input) };
        if (tool.tool === "EnterPlanMode" && input.status === "streaming") input.plan = String(object(old.input).plan ?? "") + String(object(tool.input).plan ?? "");
        tool = { ...old, ...tool, input, detail: tool.detail ?? old.detail };
      }
      return replaceTool(index, toolIndex, tool);
    }
  }
  return [...messages, message({ kind: "tool", events: [tool] })];
}
