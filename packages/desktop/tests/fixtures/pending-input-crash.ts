import { writeFileSync, readFileSync } from "node:fs";
import path from "node:path";
import { Orchestrator } from "../../electron/orchestrator";
import { ChatDatabase } from "../../electron/orchestrator-chat-db";
import { registerChatIpc } from "../../electron/orchestrator-chat";
import { CheckpointManager } from "../../electron/orchestrator-checkpoints";
import { CodexAppServerAdapter } from "../../electron/codex-app-server-adapter";
import { ClaudeCodeAdapter } from "../../electron/claude-code-adapter";

const [directory, agent, phase] = process.argv.slice(2);
if (!directory || (agent !== "codex" && agent !== "claude-code") || !["delivery", "publication", "queued", "terminal", "terminal-ack"].includes(phase)) {
  throw new Error("Expected owned fixture directory, native provider, and crash phase");
}
const repo = path.join(directory, "repo");
setTimeout(() => { throw new Error("Native crash fixture timed out"); }, 180_000).unref();
const orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), timeoutMs: 120_000, maxConcurrent: 1 });
const db = new ChatDatabase(path.join(directory, "chat.sqlite"));
orchestrator.on("orchestrator:event", ({ taskId, event }) => {
  if (phase === "terminal" && taskId === task.id && event.type === "done" && turns === 1) crash();
});
registerChatIpc({ handle() {} }, orchestrator, db, () => null);
let complete!: () => void;
const completed = new Promise<void>(resolve => { complete = resolve; });
let turns = 0;
let blockerId: string | undefined;
const task = await orchestrator.createTask("Create crash-proof.txt containing exactly before without a newline. Reply INITIAL_READY. Do not modify other files or use subagents.", agent, repo, "HEAD");
const crash = () => {
  writeFileSync(path.join(directory, "crash-state.json"), JSON.stringify({ taskId: task.id, blockerId, phase, input: task.pendingInput,
    completions: task.pendingCompletions, file: readFileSync(path.join(task.worktreePath, "crash-proof.txt"), "utf8") }));
  // Abrupt owner exit intentionally skips all finalizers. Native children lose
  // their pipes; the parent test owns this detached process group for cleanup.
  process.exit(86);
};
if (phase === "terminal-ack") {
  const acknowledge = orchestrator.acknowledgeCompletion.bind(orchestrator);
  orchestrator.acknowledgeCompletion = (taskId, id) => {
    if (taskId === task.id && turns === 1) crash();
    acknowledge(taskId, id);
  };
}
orchestrator.on("orchestrator:event", ({ taskId, event }) => {
  if (event.approval) void orchestrator.recordApproval(taskId, { requestId: event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
  if (taskId !== task.id) return;
  if (event.type !== "done") return;
  if (event.outcome !== "completed") throw new Error("Native fixture turn failed");
  if (++turns === 2) {
    complete();
    if (phase === "delivery") setImmediate(crash);
  }
});
orchestrator.on("orchestrator:task-status", (status: { taskId: string; status: string }) => {
  if (phase === "queued" && status.taskId === task.id && status.status === "queued") setImmediate(crash);
});
if (phase === "delivery") {
  const prototype = agent === "codex" ? CodexAppServerAdapter.prototype : ClaudeCodeAdapter.prototype;
  const iterate = prototype.iterate;
  prototype.iterate = async function(...args) {
    await iterate.apply(this, args);
    // The real provider accepts and executes; hold only the owner's return
    // path so its persisted input still has unknown delivery at the crash.
    await new Promise<void>(() => {});
  };
} else if (phase === "publication") {
  const record = CheckpointManager.prototype.recordCheckpoint;
  CheckpointManager.prototype.recordCheckpoint = async function(owner, number, sha) {
    if (number === 1) { await completed; crash(); }
    return record.call(this, owner, number, sha);
  };
}
await orchestrator.startTask(task.id);
while (turns < 1) await new Promise(resolve => setTimeout(resolve, 50));
if (phase === "queued") {
  const blocker = await orchestrator.createTask("Run a shell command that sleeps for 60 seconds. Then reply WAITED. Do not modify files or use subagents.", agent, repo, "HEAD");
  blockerId = blocker.id;
  await orchestrator.startTask(blocker.id);
}
await orchestrator.iterate(task.id, "Append exactly -native to crash-proof.txt without a newline. Reply FOLLOWUP_READY. Do not modify other files or use subagents.", undefined, "crash-input");
if (phase === "terminal" || phase === "terminal-ack") await completed;
throw new Error("Crash injection did not terminate its owner");
