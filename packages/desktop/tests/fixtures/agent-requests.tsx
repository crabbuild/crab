import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { AskUserCard } from "../../src/components/orchestrator/tool-cards/AskUserCard";
import { ApprovalCard } from "../../src/components/orchestrator/tool-cards/ApprovalCard";
import { HunkActionsProvider } from "../../src/components/orchestrator/tool-cards/HunkActionsContext";
import { NewChatLanding } from "../../src/components/orchestrator/NewChatLanding";
import { ChatTab } from "../../src/components/orchestrator/ChatTab";
import { CostDetailPanel } from "../../src/components/orchestrator/CostDetailPanel";
import { MultiAgentView } from "../../src/components/orchestrator/MultiAgentView";
import type { TaskStatus } from "../../src/components/orchestrator/TaskCard";
import { applyAgentEvent, applyAgentText } from "../../electron/agent-chat-messages";
import type { AgentEvent, AgentInputResponse, CodexGoal, CodexGoalMutation } from "../../electron/orchestrator-adapters";
import type { ChatMessage } from "../../electron/agent-chat-messages";
import { ChatTranscript } from "../../src/components/orchestrator/ChatTranscript";
import { TurnSummaryCard } from "../../src/components/orchestrator/TurnSummaryCard";
import { CheckpointDropdown } from "../../src/components/orchestrator/CheckpointDropdown";
import { deriveTurns } from "../../src/lib/turn-derivation";
import { useAgentChat } from "../../src/lib/use-agent-chat";
import { ToastManager } from "../../src/components/ToastManager";
import type { ChatUpdate } from "../../electron/agent-chat-messages";
import "../../src/index.css";

declare global {
  interface Window {
    requestCalls: Array<{ kind: string; taskId: string; requestId: string; response?: AgentInputResponse; decision?: string }>;
    failRequest: boolean;
    openedUrls: string[];
    modelSubmission?: { model: string | null; intelligence: string | null };
    turnSubmission?: { text: string; model?: string | null; mode?: string; intelligence?: string | null };
    modelQueries: Array<{ id: string; cancelled: boolean }>;
    resolveModels?: () => void;
    chatStreamReady?: boolean;
    resolveChat?: (snapshot: { success: boolean; messages: ChatMessage[]; revision: number; persistenceError?: string }) => void;
    updateChat?: (update: ChatUpdate) => void;
    failChat?: (error: { taskId: string | null; message: string }) => void;
    goalFixture: { calls: Array<CodexGoalMutation | null>; goal: CodexGoal | null; fail: boolean; notify: (goal: CodexGoal | null) => void };
  }
}
window.requestCalls = [];
window.failRequest = false;
window.openedUrls = [];
window.modelQueries = [];
const goalListeners = new Set<Parameters<Window["crab"]["orchestrator"]["onEvent"]>[0]>();
window.goalFixture = { calls: [], goal: null, fail: false, notify: goal => {
  window.goalFixture.goal = goal;
  goalListeners.forEach(listener => listener({ taskId: "native-turn", event: { type: "goal_updated", timestamp: Date.now(), goal } }));
} };
const liveChat = new URLSearchParams(location.search).get("liveChat");
const liveHunk = new URLSearchParams(location.search).get("liveHunk");
const liveSnapshots = new URLSearchParams(location.search).get("liveSnapshots");
const snapshotTask = new URLSearchParams(location.search).get("snapshotTask") ?? "snapshot-fixture";
const liveTask = new URLSearchParams(location.search).get("liveTask") ?? "projection-task";
const slowHistory = new URLSearchParams(location.search).has("slowHistory");
const useReplica = Boolean(liveChat || slowHistory);
Object.defineProperty(window, "crab", { value: { openUrl: async (url: string) => { window.openedUrls.push(url); }, orchestrator: {
  onEvent: (listener: Parameters<Window["crab"]["orchestrator"]["onEvent"]>[0]) => { goalListeners.add(listener); return () => { goalListeners.delete(listener); }; },
  getGoal: async () => ({ success: true, goal: window.goalFixture.goal }),
  setGoal: async (_taskId: string, mutation: CodexGoalMutation) => {
    window.goalFixture.calls.push(mutation);
    if (window.goalFixture.fail) { window.goalFixture.fail = false; return { success: false, error: "Native goal unavailable" }; }
    const old = window.goalFixture.goal;
    const goal: CodexGoal = { threadId: "native-thread", objective: "", status: "active", tokensUsed: 0, tokenBudget: null, timeUsedSeconds: 0, createdAt: 1, updatedAt: 1, ...old, ...mutation };
    window.goalFixture.notify(goal);
    return { success: true, goal };
  },
  clearGoal: async () => { window.goalFixture.calls.push(null); window.goalFixture.notify(null); return { success: true }; },
  listCheckpoints: async () => liveSnapshots ? fetch(`${liveSnapshots}/snapshots`).then(response => response.json()) : { success: true, checkpoints: [], backups: [] },
  onCheckpointCreated: () => () => {},
  onFilesChanged: () => () => {},
  restoreFiles: async (taskId: string, sourceRef: string) => fetch(`${liveSnapshots}/restore`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ taskId, sourceRef }),
  }).then(response => response.json()),
  revertHunk: async (taskId: string, patch: { filePath: string }) => {
    window.requestCalls.push({ kind: "revert", taskId, requestId: patch.filePath });
    if (liveHunk) return fetch(liveHunk, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ patch }) }).then(response => response.json());
    return { success: false, error: "File has changed since the edit" };
  },
  loadChat: async () => liveChat ? fetch(`${liveChat}/chat`).then(response => response.json()) : slowHistory
    ? new Promise(resolve => { window.resolveChat = resolve; }) : { success: true, messages: [], revision: 0 },
  onError: (listener: NonNullable<Window["failChat"]>) => {
    window.failChat = listener;
    return () => { window.failChat = undefined; };
  },
  onChatUpdate: (listener: (update: ChatUpdate) => void) => {
    window.updateChat = listener;
    if (!liveChat) return () => { window.updateChat = undefined; };
    const source = new EventSource(`${liveChat}/events`);
    source.onopen = () => { window.chatStreamReady = true; };
    source.onmessage = event => listener(JSON.parse(event.data));
    return () => { window.updateChat = undefined; window.chatStreamReady = false; source.close(); };
  },
  listModels: async (_agent: string, _cwd: string, id: string) => {
    window.modelQueries.push({ id, cancelled: false });
    if (new URLSearchParams(location.search).has("slowModels")) await new Promise<void>(resolve => { window.resolveModels = resolve; });
    return new URLSearchParams(location.search).has("failModels")
      ? { success: false, error: "Provider catalog unavailable" }
      : { success: true, models: [{ value: "runtime-model", label: "Runtime Model", intelligenceLevels: ["provider-level"], defaultIntelligence: "provider-level" }] };
  },
  cancelModelDiscovery: async (id: string) => { const query = window.modelQueries.find(query => query.id === id); if (query) query.cancelled = true; },
  getCost: async () => ({ success: true, cost: {
    workspaceId: "fixture-task", agentType: "codex", model: "provider-model", usage: [{ inputTokens: 12, outputTokens: 5, cacheReadTokens: 3, cacheWriteTokens: 0 }],
  }, turns: [{ turnId: "turn-1", timestamp: 1, inputTokens: 12, outputTokens: 5, cacheReadTokens: 3, cacheWriteTokens: 0 }] }),
  respondInput: async (taskId: string, requestId: string, response: AgentInputResponse) => {
    window.requestCalls.push({ kind: "input", taskId, requestId, response });
    if (liveChat) return fetch(`${liveChat}/input`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ taskId, requestId, response }) }).then(result => result.json());
    const success = !window.failRequest;
    window.failRequest = false;
    return { success, error: success ? undefined : "Could not send the response" };
  },
  respondApproval: async (taskId: string, payload: { requestId: string; decision: string }) => {
    window.requestCalls.push({ kind: "approval", taskId, ...payload });
    const success = !window.failRequest;
    window.failRequest = false;
    return { success, error: success ? undefined : "Connection unavailable" };
  },
} } });

function Fixture() {
  const [activeModel, setActiveModel] = useState("runtime-model");
  const [showModels, setShowModels] = useState(true);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [resolved, setResolved] = useState(false);
  const [childStatus, setChildStatus] = useState<TaskStatus>("running");
  const [openedChat, setOpenedChat] = useState("");
  const [reviewingChanges, setReviewingChanges] = useState(false);
  const [followUp, setFollowUp] = useState("");
  const initial: ChatMessage[] = [{ id: "projection-init", role: "user", content: "Projection fixture prompt", timestamp: 1 }];
  const [transcript, setTranscript] = useState<ChatMessage[]>(useReplica ? [] : initial);
  const [liveMessages] = useAgentChat(useReplica ? liveTask : undefined);
  const project = (events: AgentEvent[]) => events.reduce((messages, event, index) => applyAgentEvent(messages, event, `projection-${index}`), initial);
  return <HunkActionsProvider value={{ taskId: "fixture-task" }}>
    <ToastManager />
    <main style={{ maxWidth: 600, padding: 24, margin: "auto" }}>
      {liveSnapshots && <section aria-label="File restore fixture">
        <CheckpointDropdown task={{ id: snapshotTask, title: "Saved files", agentName: "codex", status: "reviewing", createdAt: 1, worktreeMode: "auto" }} selectedTurn={null} />
      </section>}
      <section aria-label="Undo failure fixture">
        <TurnSummaryCard turn={deriveTurns([
          { type: "file_edit", path: "first.ts", timestamp: 1, input: { old_string: "one", new_string: "two" } },
          { type: "file_edit", path: "second.ts", timestamp: 2, input: { old_string: "three", new_string: "four" } },
          { type: "done", outcome: "completed", timestamp: 3 },
        ], [])[0]} />
      </section>
      <section aria-label="Durable transcript">
        {!useReplica && <>
          <button type="button" onClick={() => setTranscript(project([
            { type: "tool_use", tool: "Bash", itemId: "first-call", input: { command: "echo first" }, timestamp: 2 },
            { type: "tool_result", itemId: "first-call", output: "first", timestamp: 3 },
          ]))}>Show completed call</button>
          <button type="button" onClick={() => setTranscript(messages => applyAgentEvent(messages,
            { type: "tool_use", tool: "Bash", itemId: "second-call", input: { command: "echo second" }, timestamp: 4 }, "second-call"))}>Start another call</button>
          <button type="button" onClick={() => setTranscript(messages => applyAgentEvent(messages,
            { type: "tool_result", itemId: "second-call", output: "second", timestamp: 5 }, "second-result"))}>Complete second call</button>
          <button type="button" onClick={() => setTranscript(project([
            { type: "turn_started", turnId: "blocks-turn", timestamp: 2 },
            { type: "text", itemId: "first", content: "Repeated", timestamp: 3 },
            { type: "text", itemId: "second", content: "Repeated", timestamp: 4 },
            { type: "done", outcome: "completed", timestamp: 5 },
          ]))}>Show separate blocks</button>
          <button type="button" onClick={() => setTranscript(project([
            { type: "turn_started", turnId: "pending-turn", timestamp: 2 },
            { type: "input_request", itemId: "pending", inputRequest: { requestId: "background-question", questions: [{ id: "mode", question: "Continue the background task?", options: [{ label: "Continue" }] }] }, timestamp: 3 },
            { type: "text", itemId: "reply", content: "Background reply", timestamp: 4 },
            { type: "done", outcome: "completed", timestamp: 5 },
          ]))}>Show background request</button>
          <button type="button" onClick={() => setTranscript(project([
            { type: "turn_started", turnId: "failed-turn", timestamp: 2 },
            { type: "tool_use", tool: "Read", itemId: "read", input: { file_path: "failure.txt" }, timestamp: 3 },
            { type: "error", error: "Provider failed fixture", timestamp: 4 },
            { type: "done", outcome: "failed", timestamp: 5 },
          ]))}>Show failed turn</button>
          <button type="button" onClick={() => setTranscript(messages => messages.map(message => ({ ...message, events: message.events?.map(tool => ({ ...tool, input: { ...(tool.input as object), resolved: true, resolutionReason: "Connection closed" } })) })))}>Close background connection</button>
        </>}
        <ChatTranscript taskId={liveTask} messages={useReplica ? liveMessages[liveTask] ?? [] : transcript} agentName="codex" />
        {liveChat && <>
          <input aria-label="Live follow-up" value={followUp} onChange={event => setFollowUp(event.target.value)} />
          <button type="button" onClick={() => { void fetch(`${liveChat}/iterate`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ feedback: followUp }) }); }}>Send follow-up</button>
        </>}
      </section>
      <AskUserCard event={{ type: "generic", tool: "AskUserQuestion", summary: "Questions", input: {
        requestId: "question-1", resolved, questions: [
          { id: "mode", question: "Which mode?", options: [{ label: "Fast" }, { label: "Thorough" }], allowFreeform: true },
          { id: "checks", question: "Which checks?", options: [{ label: "Build" }, { label: "Test" }], multiSelect: true },
          { id: "secret", question: "Fixture value?", isSecret: true, allowFreeform: true },
        ],
      } }} />
      <ApprovalCard event={{ type: "generic", tool: "CodexApproval", summary: "Run a command", input: {
        requestId: "approval-1", summary: "Run a command", kind: "command", blocking: true, resolved,
        options: [{ id: "allow_once", label: "Allow once" }, { id: "network:1", label: "Block network access to example.invalid" }, { id: "deny", label: "Deny" }],
      } }} />
      <button type="button" onClick={() => { window.failRequest = true; }}>Fail next response</button>
      <button type="button" onClick={() => setResolved(true)}>Resolve from provider</button>
      <section aria-label="Streamed replies">
        <button type="button" onClick={() => setMessages(current => [
          { type: "text" as const, timestamp: 1, itemId: "a", content: "ha" },
          { type: "text" as const, timestamp: 2, itemId: "a", content: "ha" },
          { type: "text" as const, timestamp: 3, itemId: "b", content: "haha", textMode: "replace" as const },
        ].reduce((items, event) => applyAgentText(items, event, event.itemId), current))}>Stream text</button>
        <button type="button" onClick={() => setMessages(current => applyAgentText(current,
          { type: "text", timestamp: 4, itemId: "a", content: "haha!", textMode: "replace" }, "a"))}>Complete text</button>
        {messages.map(message => <p key={message.id}>{message.content}</p>)}
      </section>
      <section aria-label="MCP form">
        <AskUserCard event={{ type: "generic", tool: "AskUserQuestion", summary: "MCP form", input: {
          kind: "elicitation", requestId: "mcp-form-1", serverName: "Fixture MCP", message: "Choose values to send", mode: "form", fields: [
            { name: "count", title: "Count", type: "integer", required: true, default: 0, minimum: 0, maximum: 5 },
            { name: "enabled", title: "Include reports", type: "boolean", required: false, default: false },
            { name: "mode", title: "Review mode", type: "string", required: true, options: [{ value: "thorough", label: "Thorough review" }] },
            { name: "checks", title: "Checks", type: "array", required: false, options: [{ value: "build", label: "Build project" }, { value: "test", label: "Run tests" }] },
            { name: "note", title: "Note", type: "string", required: true, default: "" },
          ],
        } }} />
      </section>
      <section aria-label="MCP URL">
        <AskUserCard event={{ type: "generic", tool: "AskUserQuestion", summary: "MCP URL", input: {
          kind: "elicitation", requestId: "mcp-url-1", serverName: "Fixture MCP", message: "Open the external consent page", mode: "url", url: "https://auth.example.invalid/connect?flow=fixture",
        } }} />
      </section>
      <CostDetailPanel workspaceId="fixture-task" />
      <section aria-label="Child conversations">
        <MultiAgentView task={{ id: "parent", title: "Coordinate", agentName: "claude-code", status: "reviewing", createdAt: 1 }}
          childTasks={[{ id: "child", title: "Review", spawnedRole: "reviewer", agentName: "codex", status: childStatus, createdAt: 2 }]}
          onOpenChat={setOpenedChat} onReviewChanges={() => setReviewingChanges(true)} />
        <button type="button" onClick={() => setChildStatus("reviewing")}>Complete child turn</button>
        <button type="button" onClick={() => setChildStatus("failed")}>Fail child turn</button>
        {reviewingChanges && <p>Workspace changes</p>}
        {openedChat === "child" && <HunkActionsProvider value={{ taskId: "child" }}>
          <ApprovalCard event={{ type: "generic", tool: "CodexApproval", summary: "Child permission", input: {
            requestId: "child-approval", summary: "Child permission", kind: "command", blocking: true,
            options: [{ id: "allow_once", label: "Allow child once" }, { id: "deny", label: "Deny child" }],
          } }} />
        </HunkActionsProvider>}
      </section>
      <section aria-label="Provider models">
        <button type="button" onClick={() => setShowModels(false)}>Hide model picker</button>
        <button type="button" onClick={() => window.resolveModels?.()}>Complete model discovery</button>
        {showModels && <NewChatLanding repoName="Fixture" repoPath="/fixture" agents={[{ type: "codex", name: "Codex", available: true }]} branches={["main"]}
          onSubmit={submission => { window.modelSubmission = { model: submission.model, intelligence: submission.intelligence }; }} />}
      </section>
      <section aria-label="Active chat controls" className="h-[400px]">
        <ChatTab task={{ id: "native-turn", title: "Native controls", agentName: "codex", status: "reviewing", createdAt: 1, model: "runtime-model" }} messages={[]} isStreaming={false} onStop={() => {}}
          models={[{ value: "runtime-model", label: "Runtime Model" }]} selectedModel={activeModel} onModelChange={setActiveModel} intelligenceLevels={activeModel ? ["provider-level"] : []}
          onSendMessage={(text, extras) => { window.turnSubmission = { text, ...extras }; }} />
      </section>
    </main>
  </HunkActionsProvider>;
}

const element = document.getElementById("root");
if (!element) throw new Error("Missing fixture root");
createRoot(element).render(<Fixture />);
