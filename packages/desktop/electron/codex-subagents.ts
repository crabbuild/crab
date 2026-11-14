import type { AgentEvent } from "./orchestrator-adapters";
import type { CodexRpcClient } from "./codex-rpc";
import { parseCodexEvents, type CodexParseState } from "./codex-shared";

interface Child {
  state: CodexParseState;
  pending: AgentEvent[] | null;
}

/** Subscribe to native descendants without changing their model or permissions. */
export class CodexSubagents {
  private children = new Map<string, Child>();

  constructor(private readonly rpc: CodexRpcClient, private readonly root: () => string | null,
    private readonly emit: (event: AgentEvent) => void, private readonly signal: AbortSignal) {}

  started(thread: Record<string, unknown>): void {
    const source = thread.source as { subAgent?: { thread_spawn?: { parent_thread_id?: string } } } | undefined;
    const parent = String(thread.parentThreadId ?? source?.subAgent?.thread_spawn?.parent_thread_id ?? "");
    if (parent !== this.root() && !this.children.has(parent)) return;
    const id = String(thread.id);
    const name = String(thread.agentNickname ?? thread.agentRole ?? "Sub-agent");
    this.track(id, parent, name);
    this.emit({ type: "subagent", timestamp: Date.now(), subagent: { id, parentId: parent, name } });
  }

  handle(method: string, params: Record<string, unknown>): boolean {
    const id = String(params.threadId ?? "");
    const child = this.children.get(id);
    if (id !== this.root() && !child) return false;
    const item = params.item as Record<string, unknown> | undefined;
    if (item?.type === "collabAgentToolCall" && Array.isArray(item.receiverThreadIds)) {
      for (const receiver of item.receiverThreadIds) {
        const target = String(receiver);
        if (target === this.root() || target === id) continue;
        if (item.tool === "spawnAgent") this.track(target, id, undefined, typeof item.prompt === "string" ? item.prompt : undefined);
        const state = (item.agentsStates as Record<string, { status: string; message?: string }> | undefined)?.[target];
        if (state && this.children.has(target)) {
          const status = state.status === "completed" ? "completed" : state.status === "errored" || state.status === "notFound" ? "failed"
            : state.status === "shutdown" ? "stopped" : state.status === "interrupted" ? "interrupted" : "running";
          this.emit({ type: "subagent", timestamp: Date.now(), subagent: { id: target, status } });
        }
      }
    }
    if (item?.type === "subAgentActivity") {
      this.track(String(item.agentThreadId), id, String(item.agentPath ?? "Sub-agent"));
    }
    if (!child) return false;
    const timestamp = Date.now();
    const turnId = String(params.turnId ?? (params.turn as { id?: string } | undefined)?.id ?? "");
    let events: AgentEvent[] = [];
    if (method === "turn/started") {
      events = [{ type: "turn_started", timestamp, turnId }];
      this.emit({ type: "subagent", timestamp, subagent: { id, status: "running" } });
    }
    else if (method === "turn/completed") {
      const turn = params.turn as { status: string; error?: { message?: string } };
      const outcome = turn.status === "completed" || turn.status === "interrupted" ? turn.status : "failed";
      if (outcome === "failed") events.push({ type: "error", timestamp, turnId, error: turn.error?.message ?? "Sub-agent turn failed" });
      events.push({ type: "done", timestamp, turnId, outcome });
      this.emit({ type: "subagent", timestamp, subagent: { id, status: outcome } });
    } else if (method === "item/commandExecution/outputDelta") {
      events = [{ type: "tool_result", timestamp, turnId, itemId: String(params.itemId), output: String(params.delta ?? "") }];
    } else if (method === "item/reasoning/summaryTextDelta" || method === "item/reasoning/textDelta") {
      events = [{ type: "thinking", timestamp, turnId, itemId: String(params.itemId), content: String(params.delta ?? "") }];
    } else events = parseCodexEvents({ method, params }, child.state).map(event => ({ ...event, turnId }));
    for (const event of events) {
      if (child.pending) child.pending.push(event);
      else this.emit({ type: "subagent", timestamp, subagent: { id, event } });
    }
    return true;
  }

  private track(id: string, parentId: string, name?: string, prompt?: string): void {
    if (!id || id === this.root() || this.signal.aborted) return;
    const metadata = { id, parentId, provider: "codex", ...(name ? { name } : {}), ...(prompt ? { prompt } : {}) };
    if (this.children.has(id)) {
      // Ancestor activity can mention a deeper descendant. Only the thread's
      // own started metadata may revise its parent and resolved name.
      if (prompt) this.emit({ type: "subagent", timestamp: Date.now(), subagent: { id, prompt } });
      return;
    }
    const child: Child = { state: { agentMessages: new Map(), lastAgentMessage: "" }, pending: [] };
    this.children.set(id, child);
    this.emit({ type: "subagent", timestamp: Date.now(), subagent: { ...metadata, status: "running" } });
    // thread/resume rejoins a loaded thread. Omitting overrides preserves the
    // parent's native ownership; paginated history closes the subscription gap.
    void this.hydrate(id, child).catch((error: unknown) => {
      if (this.signal.aborted) return;
      const pending = child.pending ?? [];
      child.pending = null;
      this.emit({ type: "subagent", timestamp: Date.now(), subagent: { id, event: { type: "error", timestamp: Date.now(),
        error: `Could not load sub-agent transcript: ${error instanceof Error ? error.message : String(error)}` } } });
      for (const event of pending) this.emit({ type: "subagent", timestamp: event.timestamp, subagent: { id, event } });
    });
  }

  private async hydrate(id: string, child: Child): Promise<void> {
    await this.rpc.send("thread/resume", { threadId: id, excludeTurns: true });
    const transcript: AgentEvent[] = [];
    const state: CodexParseState = { agentMessages: new Map(), lastAgentMessage: "" };
    let cursor: string | undefined;
    const completedTurns = new Set<string>();
    const completedItems = new Set<string>();
    const startedTurns = new Set<string>();
    let status: NonNullable<AgentEvent["subagent"]>["status"];
    do {
      this.signal.throwIfAborted();
      const page = await this.rpc.send("thread/turns/list", { threadId: id, cursor, itemsView: "full", sortDirection: "asc" }) as {
        data: Array<{ id: string; status: string; error?: { message?: string }; items: Array<Record<string, unknown>> }>; nextCursor: string | null;
      };
      for (const turn of page.data) {
        const timestamp = Date.now();
        status = turn.status === "inProgress" ? "running" : turn.status === "completed" || turn.status === "interrupted" ? turn.status : "failed";
        transcript.push({ type: "turn_started", timestamp, turnId: turn.id });
        startedTurns.add(turn.id);
        for (const item of turn.items) {
          if (item.type === "collabAgentToolCall" || item.type === "subAgentActivity") this.handle("item/completed", { threadId: id, turnId: turn.id, item });
          // An active text item has no stream offset. Replay live deltas only;
          // its item/completed snapshot later supplies any missed prefix.
          if (turn.status === "inProgress" && (item.type === "agentMessage" || item.status === "inProgress")) continue;
          completedItems.add(String(item.id));
          if (item.type === "userMessage") {
            const content = (item.content as Array<{ text?: string }>).map(block => block.text ?? "").join("\n");
            transcript.push({ type: "user_message", timestamp, turnId: turn.id, content });
          } else {
            const params = { threadId: id, turnId: turn.id, item };
            const started = parseCodexEvents({ method: "item/started", params }, state);
            const completed = item.status === "inProgress" ? [] : parseCodexEvents({ method: "item/completed", params }, state);
            transcript.push(...[...started, ...completed].map(event => ({ ...event, turnId: turn.id })));
          }
        }
        if (status === "failed") transcript.push({ type: "error", timestamp, turnId: turn.id, error: turn.error?.message ?? "Sub-agent turn failed" });
        if (turn.status !== "inProgress") transcript.push({ type: "done", timestamp, turnId: turn.id,
          outcome: turn.status === "completed" || turn.status === "interrupted" ? turn.status : "failed" });
        if (turn.status !== "inProgress") completedTurns.add(turn.id);
      }
      cursor = page.nextCursor ?? undefined;
    } while (cursor);
    this.signal.throwIfAborted();
    const pending = child.pending ?? [];
    child.pending = null;
    const replay = pending.filter(event => !(event.turnId && completedTurns.has(event.turnId) || event.itemId && completedItems.has(event.itemId)
      || event.type === "turn_started" && event.turnId && startedTurns.has(event.turnId)));
    for (const event of replay) {
      if (event.type === "turn_started") status = "running";
      if (event.type === "done") status = event.outcome ?? "completed";
    }
    this.emit({ type: "subagent", timestamp: Date.now(), subagent: { id, transcript, ...(status ? { status } : {}) } });
    for (const event of replay) {
      this.emit({ type: "subagent", timestamp: event.timestamp, subagent: { id, event } });
    }
  }
}
