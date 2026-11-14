import type { Orchestrator, Task } from "./orchestrator";
import type { ChatMessage } from "./agent-chat-messages";
import { AgentSubmissionUncertainError } from "./orchestrator-adapters";

export interface AgentTeamTools {
  tools: Array<{ name: string; description: string; inputSchema: Record<string, unknown> }>;
  call(name: string, input: unknown, signal: AbortSignal): Promise<unknown>;
}

const string = { type: "string", minLength: 1 };
const agentId = { agent_id: string };
const definitions = [
  { name: "crab_spawn_agent", description: "Delegate a focused task to Codex or Claude Code in this shared workspace. Returns immediately with an agent_id; use crab_wait_agent and crab_read_agent for results. Coordinate file ownership to avoid concurrent edits.",
    properties: { provider: { enum: ["codex", "claude-code"] }, prompt: string, name: string }, required: ["provider", "prompt", "name"] },
  { name: "crab_list_agents", description: "List this conversation's team, including each agent's provider, parent, and current status.", properties: {}, required: [] },
  { name: "crab_send_message", description: "Send a message to a parent, child, or sibling in this team. Active agents receive native steering; idle agents start a follow-up turn. Returns only after delivery acknowledgement. Do not wait for a reply while the recipient is waiting for you.",
    properties: { ...agentId, message: string }, required: ["agent_id", "message"] },
  { name: "crab_read_agent", description: "Read an agent's complete saved transcript, including tools and results. No output is truncated.", properties: agentId, required: ["agent_id"] },
  { name: "crab_wait_agent", description: "Wait up to 30 seconds for an agent's current turn to finish. Returns its real status, including queued or still running. Read the complete result with crab_read_agent.",
    properties: { ...agentId, timeout_seconds: { type: "number", minimum: 0, maximum: 30 } }, required: ["agent_id"] },
  { name: "crab_stop_agent", description: "Stop one of your descendants and its children, preserving their transcripts and shared workspace.", properties: agentId, required: ["agent_id"] },
];

function inputObject(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Tool input must be an object");
  return value as Record<string, unknown>;
}

function field(input: Record<string, unknown>, name: string): string {
  const value = input[name];
  if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a non-empty string`);
  return value;
}

function summary(task: Task) {
  return { agent_id: task.id, name: task.spawnedRole ?? task.title, provider: task.agentType, parent_id: task.parentTaskId, status: task.status };
}

/** Bind tools to an owner; provider-supplied IDs never select a different team. */
export function createAgentTeamTools(owner: Orchestrator, task: Task, read: (id: string) => ChatMessage[]): AgentTeamTools {
  const root = () => owner.getTask(task.id)?.rootTaskId ?? task.id;
  const target = (id: string): Task => {
    const peer = owner.getTask(id);
    if (!peer || (peer.rootTaskId ?? peer.id) !== root()) throw new Error("Agent does not belong to this team");
    return peer;
  };
  return {
    tools: definitions.map(({ name, description, properties, required }) => ({ name, description,
      inputSchema: { type: "object", properties, required, additionalProperties: false } })),
    async call(name, value, signal) {
      signal.throwIfAborted();
      const input = inputObject(value);
      if (name === "crab_list_agents") return owner.getAllTasks().filter(peer => (peer.rootTaskId ?? peer.id) === root()).map(summary);
      if (name === "crab_spawn_agent") {
        const provider = field(input, "provider");
        if (provider !== "codex" && provider !== "claude-code") throw new Error("provider must be codex or claude-code");
        const child = owner.createChildTaskForAgent(task.id, provider, field(input, "prompt"), field(input, "name"));
        try { await owner.persistTasks(); } catch (error) { await owner.removeTask(child.id); throw error; }
        if (signal.aborted) { await owner.cancelTask(child.id); signal.throwIfAborted(); }
        // Startup may ask for approval. Return the durable identity before
        // waiting for admission or a provider acknowledgement.
        void owner.startTask(child.id).catch((error: unknown) => {
          owner.emit("orchestrator:error", { taskId: child.id, message: error instanceof Error ? error.message : String(error) });
        });
        return summary(child);
      }
      const peer = target(field(input, "agent_id"));
      if (name === "crab_read_agent") return { ...summary(peer), messages: read(peer.id) };
      if (peer.id === task.id) throw new Error("Choose another agent");
      if (name === "crab_send_message") {
        const content = field(input, "message");
        let delivery: "accepted" | "unconfirmed" | "failed" = "accepted";
        try {
          await owner.iterate(peer.id, `Message from agent ${task.id} (${task.spawnedRole ?? task.agentType}):\n\n${content}`);
        } catch (error) {
          delivery = error instanceof AgentSubmissionUncertainError ? "unconfirmed" : "failed";
          throw error;
        } finally {
          const event = { type: "agent_message" as const, timestamp: Date.now(), agentMessage: { from: task.id, to: peer.id, content, delivery } };
          owner.recordTeamEvent(task.id, event);
          owner.recordTeamEvent(peer.id, event);
        }
        return { ...summary(peer), delivery };
      }
      if (name === "crab_stop_agent") {
        let ancestor = peer.parentTaskId;
        while (ancestor && ancestor !== task.id) ancestor = owner.getTask(ancestor)?.parentTaskId;
        if (ancestor !== task.id) throw new Error("An agent can only stop its own descendants");
        await owner.cancelTask(peer.id);
        return summary(peer);
      }
      if (name !== "crab_wait_agent") throw new Error(`Unknown team tool: ${name}`);
      const seconds = input.timeout_seconds ?? 30;
      if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds < 0 || seconds > 30) throw new Error("timeout_seconds must be between 0 and 30");
      const active = () => ["created", "queued", "running", "iterating"].includes(peer.status);
      if (active() && seconds > 0) await new Promise<void>((resolve, reject) => {
        const finish = () => { clearTimeout(timer); owner.off("orchestrator:task-status", changed); signal.removeEventListener("abort", abort); };
        const changed = () => { if (!active()) { finish(); resolve(); } };
        const abort = () => { finish(); reject(signal.reason); };
        const timer = setTimeout(() => { finish(); resolve(); }, seconds * 1000);
        owner.on("orchestrator:task-status", changed);
        signal.addEventListener("abort", abort, { once: true });
        if (signal.aborted) abort(); else changed();
      });
      return summary(peer);
    },
  };
}

/** Serve the SDK MCP bridge over Claude's existing control transport. */
export async function teamMcpResponse(tools: AgentTeamTools, message: Record<string, unknown>, signal: AbortSignal): Promise<object> {
  const base = { jsonrpc: "2.0", id: message.id };
  if (message.method === "initialize") return { ...base, result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "crab_team", version: "1.0.0" } } };
  if (message.method === "notifications/initialized" || message.method === "ping") return { ...base, result: {} };
  if (message.method === "tools/list") return { ...base, result: { tools: tools.tools } };
  if (message.method !== "tools/call") return { ...base, error: { code: -32601, message: "Unknown MCP method" } };
  try {
    const params = inputObject(message.params);
    const result = await tools.call(field(params, "name"), params.arguments, signal);
    return { ...base, result: { content: [{ type: "text", text: JSON.stringify(result) }] } };
  } catch (error) {
    return { ...base, result: { isError: true, content: [{ type: "text", text: error instanceof Error ? error.message : String(error) }] } };
  }
}
