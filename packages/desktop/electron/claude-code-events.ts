import type { AgentEvent } from "./orchestrator-adapters";
import { pathVariants } from "./orchestrator-adapters";

/** Parse the complete and partial messages emitted by Claude Code stream-json. */
export class ClaudeEventParser {
  private sessionId: string | undefined;
  private messageIds = new Map<string, string>();
  private streamed = new Map<string, Array<{ index: number; type: "text" | "thinking"; content: string; consumed: boolean }>>();
  private seenAssistantMessages = new Set<string>();
  private agents = new Map<string, { parentId?: string; background?: boolean }>();
  private toolAgents = new Map<string, string>();
  private messages = new Map<string, { from: string; to: string; content: string }>();
  private textAgents = new Set<string>();
  private resultAgents = new Set<string>();

  constructor(private readonly cwd: string) {}

  parse(raw: Record<string, unknown>): AgentEvent[] {
    const timestamp = Date.now();
    const parentToolId = typeof raw.parent_tool_use_id === "string" ? raw.parent_tool_use_id : undefined;
    const agentId = typeof raw.agent_id === "string" ? raw.agent_id : parentToolId ? this.toolAgents.get(parentToolId) ?? parentToolId : undefined;
    if (raw.type === "system" && typeof raw.task_id === "string") {
      const id = raw.task_id;
      if (raw.subtype === "task_started" && raw.task_type === "local_agent" && id) {
        const toolCallId = typeof raw.tool_use_id === "string" ? raw.tool_use_id : undefined;
        const previous = this.agents.get(id) ?? (toolCallId ? this.agents.get(toolCallId) : undefined);
        if (toolCallId) { this.toolAgents.set(toolCallId, id); this.agents.delete(toolCallId); }
        const parentId = typeof raw.parent_task_id === "string" ? raw.parent_task_id : previous?.parentId;
        this.agents.set(id, { parentId, background: raw.is_backgrounded === true });
        return [{ type: "subagent", timestamp, subagent: { id, toolCallId, parentId, provider: "claude-code", status: "running", ...(raw.description ? { name: String(raw.description) } : {}) } }];
      }
      if (id && this.agents.has(id) && raw.subtype === "task_notification") {
        const status = raw.status === "failed" ? "failed" : raw.status === "stopped" ? "stopped" : "completed";
        if (!this.textAgents.has(id)) this.resultAgents.add(id);
        return [{ type: "subagent", timestamp, subagent: { id, status, event: status === "failed" || !this.textAgents.has(id) ? {
          type: status === "failed" ? "error" : "text", timestamp, itemId: `agent-result:${id}`,
          content: String(raw.summary ?? ""), error: status === "failed" ? String(raw.summary ?? "Sub-agent failed") : undefined, textMode: "replace",
        } : undefined } }];
      }
    }
    const result: AgentEvent[] = [];
    if (agentId && !this.agents.has(agentId)) {
      this.agents.set(agentId, {});
      if (parentToolId) this.toolAgents.set(parentToolId, agentId);
      result.push({ type: "subagent", timestamp, subagent: { id: agentId, toolCallId: parentToolId, provider: "claude-code", status: "running" } });
    }
    const scope = (event: AgentEvent): AgentEvent => agentId && event.type !== "session_started"
      ? { type: "subagent", timestamp: event.timestamp, subagent: { id: agentId, event } } : event;
    for (const event of this.parseMessage(raw)) {
      if (agentId && event.type === "text" && event.content) {
        // Forwarded blocks are the transcript. A notification/tool hand-back
        // is a summary of that output, even when it arrived first.
        if (this.resultAgents.delete(agentId)) result.push(scope({ type: "text", timestamp, itemId: `agent-result:${agentId}`, content: "", textMode: "replace" }));
        this.textAgents.add(agentId);
      }
      result.push(scope(event));
      if (event.tool === "SendMessage" && event.itemId) {
        const input = event.input as Record<string, unknown>;
        this.messages.set(event.itemId, { from: agentId ?? "Lead agent", to: String(input.to ?? input.recipient ?? "Team"), content: String(input.message ?? input.content ?? "") });
      }
      const sent = event.itemId ? this.messages.get(event.itemId) : undefined;
      if (event.type === "tool_result" && sent) {
        result.push(scope({ type: "agent_message", timestamp, itemId: event.itemId, agentMessage: { ...sent, delivery: event.error ? "failed" : "accepted" } }));
        this.messages.delete(event.itemId!);
      }
      if ((event.tool === "Agent" || event.tool === "Task") && event.type === "task" && event.itemId) {
        const input = event.input as Record<string, unknown> | undefined;
        const id = typeof input?.resume === "string" ? input.resume : event.itemId;
        this.toolAgents.set(event.itemId, id);
        this.agents.set(id, { parentId: agentId, background: input?.run_in_background === true });
        result.push({ type: "subagent", timestamp, subagent: { id, toolCallId: event.itemId, parentId: agentId, provider: "claude-code",
          name: String(input?.name ?? input?.description ?? input?.subagent_type ?? "Sub-agent"), prompt: String(input?.prompt ?? ""), status: "starting" } });
      }
      const targetId = event.itemId ? this.toolAgents.get(event.itemId) ?? event.itemId : undefined;
      const agent = targetId ? this.agents.get(targetId) : undefined;
      if (event.type === "tool_result" && agent && targetId && !agent.background) {
        const content = typeof event.output === "string" ? event.output : Array.isArray(event.output)
          ? event.output.map(block => String(block?.text ?? "")).join("\n") : "";
        if (!this.textAgents.has(targetId)) this.resultAgents.add(targetId);
        result.push({ type: "subagent", timestamp, subagent: { id: targetId, status: event.error ? "failed" : "completed",
          event: event.error || !this.textAgents.has(targetId) ? { type: event.error ? "error" : "text", timestamp,
            itemId: `agent-result:${targetId}`, content, error: event.error, textMode: "replace" } : undefined } });
      }
    }
    return result;
  }

  private parseMessage(raw: Record<string, unknown>): AgentEvent[] {
    const timestamp = Date.now();
    const events: AgentEvent[] = [];
    if (typeof raw.session_id === "string" && raw.session_id && raw.session_id !== this.sessionId) {
      this.sessionId = raw.session_id;
      events.push({ type: "session_started", timestamp, sessionId: this.sessionId });
    }
    if (raw.type === "stream_event") {
      const event = raw.event as Record<string, unknown> | undefined;
      const parent = String(raw.parent_tool_use_id ?? "main");
      if (event?.type === "message_start") {
        this.messageIds.set(parent, String((event.message as Record<string, unknown> | undefined)?.id ?? ""));
      }
      const block = (event?.type === "content_block_start" ? event.content_block : event?.delta) as Record<string, unknown> | undefined;
      if ((event?.type === "content_block_delta" || event?.type === "content_block_start")
        && ["text", "text_delta", "thinking", "thinking_delta"].includes(String(block?.type))) {
        const type = String(block?.type).startsWith("text") ? "text" : "thinking";
        const messageId = this.messageIds.get(parent) ?? "";
        const blocks = this.streamed.get(messageId) ?? [];
        let streamed = blocks.find(block => block.index === event.index);
        if (!streamed) { streamed = { index: Number(event.index), type, content: "", consumed: false }; blocks.push(streamed); }
        const content = String(block?.text ?? block?.thinking ?? "");
        streamed.content += content;
        this.streamed.set(messageId, blocks);
        if (content) events.push({ type, timestamp, content, itemId: `${messageId}:${event.index}` });
      }
      return events;
    }
    if (raw.type === "assistant" || raw.type === "user") {
      const message = raw.message as Record<string, unknown> | undefined;
      const blocks = message?.content;
      if (!Array.isArray(blocks)) return events;
      if (raw.type === "assistant" && typeof raw.uuid === "string") {
        if (this.seenAssistantMessages.has(raw.uuid)) return events;
        this.seenAssistantMessages.add(raw.uuid);
      }
      const streamed = this.streamed.get(String(message?.id ?? ""));
      for (const [index, block] of (blocks as Array<Record<string, unknown>>).entries()) {
        if (raw.type === "assistant" && (block.type === "text" || block.type === "thinking")) {
          const type = block.type === "text" ? "text" : "thinking";
          const content = String(block.text ?? block.thinking ?? "");
          // The SDK emits one assistant envelope per non-empty block, all
          // sharing a message ID. Pair each with its own partial block.
          const partial = streamed?.find(block => block.type === type && !block.consumed);
          if (partial) partial.consumed = true;
          if (!partial || partial.content !== content) events.push({ type, timestamp, content,
            itemId: partial ? `${message?.id}:${partial.index}` : `${raw.uuid}:${index}`, textMode: "replace" });
        } else if (raw.type === "assistant" && block.type === "tool_use") {
          events.push(this.toolUse(block, timestamp));
        } else if (raw.type === "user" && block.type === "tool_result") {
          events.push({ type: "tool_result", timestamp, itemId: String(block.tool_use_id), output: block.content,
            input: { final: true },
            error: block.is_error ? JSON.stringify(block.content) : undefined });
        }
      }
      return events;
    }
    if (raw.type !== "result") return events;

    this.streamed.clear();
    this.messageIds.clear();
    this.seenAssistantMessages.clear();
    const modelUsage = raw.modelUsage as Record<string, Record<string, unknown>> | undefined;
    for (const [model, usage] of Object.entries(modelUsage ?? {})) {
      events.push({ type: "usage", timestamp, turnId: String(raw.uuid), usage: {
        inputTokens: Number(usage.inputTokens ?? 0),
        outputTokens: Number(usage.outputTokens ?? 0),
        cacheCreationInputTokens: Number(usage.cacheCreationInputTokens ?? 0),
        cacheReadInputTokens: Number(usage.cacheReadInputTokens ?? 0),
        model,
        cumulative: { sessionId: String(raw.session_id), costUsd: Number(usage.costUSD) },
      } });
    }
    const failed = raw.is_error === true || String(raw.subtype).startsWith("error");
    if (failed) {
      const errors = Array.isArray(raw.errors) ? raw.errors.map(String).join("\n") : "";
      events.push({ type: "error", timestamp, error: String(errors || raw.result || "Claude Code turn failed") });
    }
    events.push({ type: "done", timestamp, outcome: failed ? "failed" : "completed", content: failed ? "" : String(raw.result ?? "") });
    return events;
  }

  private toolUse(block: Record<string, unknown>, timestamp: number): AgentEvent {
    const tool = String(block.name ?? "");
    const input = block.input as Record<string, unknown> | undefined;
    const event = { timestamp, tool, input, itemId: String(block.id ?? "") };
    const path = this.relative(String(input?.file_path ?? input?.path ?? input?.notebook_path ?? ""));
    if (["Write", "Edit", "NotebookEdit"].includes(tool)) {
      return { ...event, type: "file_edit", path, diff: String(input?.content ?? input?.new_string ?? input?.new_source ?? "") };
    }
    if (tool === "Read") return { ...event, type: "file_read", path };
    if (tool === "Grep" || tool === "Glob") return { ...event, type: "search", path: path || String(input?.pattern ?? "") };
    if (tool === "WebFetch" || tool === "WebSearch") return { ...event, type: "web", path: String(input?.url ?? input?.query ?? "") };
    if (tool === "Bash") return { ...event, type: "command", command: this.relative(String(input?.command ?? "")) };
    if (["Agent", "Task", "TaskOutput", "TaskStop", "TodoWrite"].includes(tool)) {
      return { ...event, type: "task", path: String(input?.description ?? input?.task_id ?? "") };
    }
    return { ...event, type: "generic" };
  }

  private relative(value: string): string {
    if (!this.cwd) return value;
    for (const variant of pathVariants(this.cwd)) value = value.split(`${variant}/`).join("");
    return value;
  }
}
