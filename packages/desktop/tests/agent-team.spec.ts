import { EventEmitter } from "node:events";
import { describe, expect, it, vi } from "vitest";
import { createAgentTeamTools, teamMcpResponse } from "../electron/agent-team";
import { applyAgentEvent, type ChatMessage } from "../electron/agent-chat-messages";
import { AgentSubmissionUncertainError, type AgentEvent } from "../electron/orchestrator-adapters";
import { ClaudeEventParser } from "../electron/claude-code-events";
import { CodexSubagents } from "../electron/codex-subagents";
import type { CodexRpcClient } from "../electron/codex-rpc";
import type { Orchestrator, Task } from "../electron/orchestrator";

function fixture() {
  const tasks = [
    { id: "lead", title: "Lead", agentType: "codex", status: "running" },
    { id: "child", parentTaskId: "lead", rootTaskId: "lead", title: "Review", agentType: "claude-code", status: "running" },
    { id: "other", title: "Other workspace", agentType: "codex", status: "running" },
  ] as Task[];
  const owner = Object.assign(new EventEmitter(), {
    getTask: (id: string) => tasks.find(task => task.id === id), getAllTasks: () => tasks,
    iterate: vi.fn(async () => {}), cancelTask: vi.fn(async () => {}), recordTeamEvent: vi.fn(),
    persistTasks: vi.fn(async () => {}), removeTask: vi.fn(async () => {}),
    startTask: vi.fn(async () => {}), createChildTaskForAgent: vi.fn(() => tasks[1]),
  });
  const read = vi.fn(() => [{ id: "output", role: "agent", kind: "text", content: "Complete output", timestamp: 1 }] as ChatMessage[]);
  const tools = createAgentTeamTools(owner as unknown as Orchestrator, tasks[0], read);
  return { owner, tasks, tools, read, signal: new AbortController().signal };
}

describe("mixed-provider team ownership", () => {
  it("starts a durable child through its parent and reads the journal without truncation", async () => {
    const { owner, tools, read, signal } = fixture();
    await tools.call("crab_spawn_agent", { provider: "claude-code", name: "Reviewer", prompt: "Review file ownership" }, signal);
    const result = await tools.call("crab_read_agent", { agent_id: "child" }, signal);
    expect({ spawn: owner.createChildTaskForAgent.mock.calls, started: owner.startTask.mock.calls, read: read.mock.calls, result })
      .toEqual({ spawn: [["lead", "claude-code", "Review file ownership", "Reviewer"]], started: [["child"]], read: [["child"]],
        result: { agent_id: "child", name: "Review", provider: "claude-code", parent_id: "lead", status: "running", messages: read.mock.results[0].value } });
  });

  it("rejects cross-team reads, messages, and cancellation before side effects", async () => {
    const { tools, owner, read, signal } = fixture();
    for (const name of ["crab_read_agent", "crab_send_message", "crab_stop_agent"]) {
      await expect(tools.call(name, { agent_id: "other", message: "Hello" }, signal)).rejects.toThrow("does not belong");
    }
    expect([owner.iterate.mock.calls, owner.cancelTask.mock.calls, read.mock.calls]).toEqual([[], [], []]);
  });

  it("keeps uncertain delivery distinct from acknowledgement in both journals", async () => {
    const { tools, owner, signal } = fixture();
    owner.iterate.mockRejectedValueOnce(new AgentSubmissionUncertainError(new Error("Connection lost")));
    await expect(tools.call("crab_send_message", { agent_id: "child", message: "Check the lease owner" }, signal)).rejects.toThrow("could not be confirmed");
    expect(owner.recordTeamEvent.mock.calls.map(([id, event]) => [id, event.agentMessage.delivery])).toEqual([["lead", "unconfirmed"], ["child", "unconfirmed"]]);
  });

  it("cleans up a pending wait on cancellation", async () => {
    const { tools, owner } = fixture();
    const controller = new AbortController();
    const waiting = tools.call("crab_wait_agent", { agent_id: "child" }, controller.signal);
    controller.abort(new Error("Stopped"));
    await expect(waiting).rejects.toThrow("Stopped");
    expect(owner.listenerCount("orchestrator:task-status")).toBe(0);
  });

  it("returns MCP tool errors as visible failures, preserving the control request", async () => {
    const { tools, signal } = fixture();
    const result = await teamMcpResponse(tools, { id: 7, method: "tools/call", params: { name: "crab_stop_agent", arguments: { agent_id: "other" } } }, signal);
    expect(result).toEqual({ jsonrpc: "2.0", id: 7, result: { isError: true, content: [{ type: "text", text: "Agent does not belong to this team" }] } });
  });
});

describe("sub-agent transcript projection", () => {
  it("isolates concurrent identical item IDs and retains every byte through a final replacement", () => {
    const output = "complete output ".repeat(600) + "END_OF_RESULT";
    const events: AgentEvent[] = [
      { type: "turn_started", timestamp: 1, turnId: "parent" },
      ...["a", "b"].map(id => ({ type: "subagent" as const, timestamp: 2, subagent: { id, event: { type: "text" as const, timestamp: 2, itemId: "same", content: id } } })),
      { type: "subagent", timestamp: 3, subagent: { id: "a", event: { type: "text", timestamp: 3, itemId: "same", content: output, textMode: "replace" } } },
      { type: "subagent", timestamp: 4, subagent: { id: "a", event: { type: "done", timestamp: 4 } } },
      { type: "text", timestamp: 5, content: "Parent continues" },
    ];
    const messages = events.reduce((messages, event, index) => applyAgentEvent(messages, event, String(index)), [] as ChatMessage[]);
    expect({ a: messages.find(message => message.subagent?.id === "a")?.messages?.[0].content,
      b: messages.find(message => message.subagent?.id === "b")?.messages?.[0].content,
      parent: messages.filter(message => message.kind === "text").map(message => message.content), done: messages.some(message => message.kind === "done") })
      .toEqual({ a: output, b: "b", parent: ["Parent continues"], done: false });
  });

  it("attributes resumed Claude agents and nested children to their original identities", () => {
    const parser = new ClaudeEventParser("");
    const raw = [
      { type: "assistant", message: { content: [{ type: "tool_use", id: "first", name: "Agent", input: { prompt: "Review" } }] } },
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "stable", tool_use_id: "first" },
      { type: "assistant", message: { content: [{ type: "tool_use", id: "resumed", name: "Agent", input: { prompt: "Continue review", resume: "stable" } }] } },
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "stable", tool_use_id: "resumed" },
      { type: "assistant", parent_tool_use_id: "first", agent_id: "stable", message: { content: [{ type: "tool_use", id: "nested", name: "Agent", input: { prompt: "Check" } }] } },
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "nested-stable", tool_use_id: "nested", parent_task_id: "stable" },
      { type: "assistant", parent_tool_use_id: "first", agent_id: "stable", uuid: "result", message: { content: [{ type: "text", text: "Resumed output" }] } },
    ];
    const events = raw.flatMap(event => parser.parse(event));
    const messages = events.reduce((messages, event, index) => applyAgentEvent(messages, event, String(index)), [] as ChatMessage[]);
    const agents = messages.filter(message => message.subagent);
    expect({ ids: agents.map(message => message.subagent?.id), nestedParent: agents.find(message => message.subagent?.id === "nested-stable")?.subagent?.parentId,
      output: agents.find(message => message.subagent?.id === "stable")?.messages?.filter(message => message.kind === "text").map(message => message.content) })
      .toEqual({ ids: ["stable", "nested-stable"], nestedParent: "stable", output: ["Resumed output"] });
  });

  it.each([true, false])("renders a forwarded Claude reply once when notification arrives first: %s", notificationFirst => {
    const parser = new ClaudeEventParser("");
    const reply = { type: "assistant", uuid: "reply", agent_id: "stable", parent_tool_use_id: "spawn", message: { content: [{ type: "text", text: "Complete findings" }] } };
    const notification = { type: "system", subtype: "task_notification", task_id: "stable", status: "completed", summary: "Complete findings" };
    const raw = [
      { type: "assistant", message: { content: [{ type: "tool_use", id: "spawn", name: "Agent", input: { prompt: "Review" } }] } },
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "stable", tool_use_id: "spawn" },
      ...(notificationFirst ? [notification, reply] : [reply, notification]),
      { type: "user", message: { content: [{ type: "tool_result", tool_use_id: "spawn", content: "Provider hand-back: Complete findings" }] } },
    ];
    const messages = raw.flatMap(event => parser.parse(event)).reduce((messages, event, index) => applyAgentEvent(messages, event, String(index)), [] as ChatMessage[]);
    expect(messages.find(message => message.subagent?.id === "stable")?.messages?.filter(message => message.kind === "text" && message.content).map(message => message.content))
      .toEqual(["Complete findings"]);
  });

  it("hydrates all Codex history pages and does not complete the parent", async () => {
    const events: AgentEvent[] = [];
    const send = vi.fn(async (method: string, params: { cursor?: string }) => method === "thread/resume" ? {} : params.cursor
      ? { data: [{ id: "two", status: "completed", items: [{ type: "agentMessage", id: "second", text: "Tail" }] }], nextCursor: null }
      : { data: [{ id: "one", status: "completed", items: [{ type: "agentMessage", id: "first", text: "First" }] }], nextCursor: "next" });
    const monitor = new CodexSubagents({ send } as unknown as CodexRpcClient, () => "root", event => events.push(event), new AbortController().signal);
    monitor.started({ id: "child", parentThreadId: "root", agentNickname: "Reviewer" });
    await vi.waitFor(() => expect(events.some(event => event.subagent?.transcript)).toBe(true));
    monitor.handle("turn/completed", { threadId: "child", turn: { id: "two", status: "completed" } });
    expect({ texts: events.find(event => event.subagent?.transcript)?.subagent?.transcript?.filter(event => event.type === "text").map(event => event.content),
      parentDone: events.some(event => event.type === "done"), last: events.at(-1)?.subagent?.event?.outcome }).toEqual({ texts: ["First", "Tail"], parentDone: false, last: "completed" });
  });

  it("keeps native Codex parentage when an ancestor reports descendant activity", async () => {
    const events: AgentEvent[] = [];
    const send = vi.fn(async (method: string) => method === "thread/resume" ? {} : { data: [], nextCursor: null });
    const monitor = new CodexSubagents({ send } as unknown as CodexRpcClient, () => "root", event => events.push(event), new AbortController().signal);
    monitor.started({ id: "child", parentThreadId: "root", agentNickname: "Reviewer" });
    monitor.started({ id: "grandchild", parentThreadId: "child", agentNickname: "Verifier" });
    monitor.handle("item/completed", { threadId: "root", item: { type: "subAgentActivity", agentThreadId: "grandchild", agentPath: "/root/reviewer/verifier", kind: "completed" } });
    await vi.waitFor(() => expect(send).toHaveBeenCalledTimes(4));
    const messages = events.reduce((messages, event, index) => applyAgentEvent(messages, event, String(index)), [] as ChatMessage[]);
    expect(messages.find(message => message.subagent?.id === "grandchild")?.subagent)
      .toMatchObject({ name: "Verifier", parentId: "child" });
  });
});
