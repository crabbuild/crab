const { app } = require("electron");
const { readFile, writeFile } = require("node:fs/promises");
const path = require("node:path");
const { setTimeout: delay } = require("node:timers/promises");
const [directory, agent, desktop] = process.argv.slice(2);
if (!directory || !["codex", "claude-code"].includes(agent) || !desktop) throw new Error("Expected owned directory, provider and desktop build");
const { Orchestrator } = require(path.join(desktop, "dist-electron/orchestrator.cjs"));
const { ChatDatabase } = require(path.join(desktop, "dist-electron/orchestrator-chat-db.cjs"));
const { registerChatIpc } = require(path.join(desktop, "dist-electron/orchestrator-chat.cjs"));
const { registerAppShutdown } = require(path.join(desktop, "dist-electron/app-shutdown.cjs"));
app.setPath("userData", path.join(directory, "electron"));
const events = [];
const quitPrevented = [];
const exits = [];
const waitFor = async predicate => {
  const deadline = Date.now() + 120_000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("Native shutdown fixture timed out");
    await delay(50);
  }
};
let orchestrator;
let db;
const main = async () => {
  await app.whenReady();
  const repo = path.join(directory, "repo");
  const args = [path.join(desktop, "tests/fixtures/mcp-elicitation-server.mjs"), path.join(directory, "mcp-proof.json"), "tool", "form"];
  const extraArgs = agent === "codex"
    ? ["-c", `mcp_servers={compatibility_fixture={command=${JSON.stringify(process.execPath)},args=${JSON.stringify(args)},env={ELECTRON_RUN_AS_NODE="1"}}}`, "-c", 'approval_policy="on-request"']
    : ["--mcp-config", JSON.stringify({ mcpServers: { compatibility_fixture: { command: process.execPath, args, env: { ELECTRON_RUN_AS_NODE: "1" } } } }), "--strict-mcp-config"];
  await writeFile(process.env.CRAB_DESKTOP_SETTINGS_PATH, JSON.stringify({ orchestratorAgentConfigs: { [agent]: { extraArgs } } }));
  orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), maxConcurrent: 1, timeoutMs: 150_000 });
  db = new ChatDatabase(path.join(directory, "chat.sqlite"));
  const handlers = new Map();
  const chat = registerChatIpc({ handle: (channel, handler) => handlers.set(channel, handler) }, orchestrator, db, () => null);
  let form;
  orchestrator.on("orchestrator:event", ({ taskId, event }) => {
    events.push({ taskId, event });
    if (event.approval) void orchestrator.recordApproval(taskId, { requestId: event.approval.requestId, decision: "allow_once", summary: "Owned quit fixture" });
    const input = event.inputRequest;
    if (input?.kind !== "elicitation" || input.serverName !== "compatibility_fixture") return;
    if (!input.fields?.length) orchestrator.recordInput(taskId, input.requestId, { kind: "elicitation", action: "accept", content: {} });
    else form = { taskId, requestId: input.requestId };
  });
  const idle = await orchestrator.createTask("Create before-shutdown.txt containing exactly ready without a newline. Reply READY. Do not modify other files or use subagents.", agent, repo, "HEAD", { worktreeMode: "primary" });
  await orchestrator.startTask(idle.id);
  await waitFor(() => idle.status === "reviewing");
  const prompt = "Call the compatibility_fixture confirm_values MCP tool once and wait for the user's response. Do not use other tools, modify files, or use subagents.";
  const active = await orchestrator.createTask(prompt, agent, repo, "HEAD", { worktreeMode: "existing", worktreePath: repo });
  if (agent === "codex") await orchestrator.setGoal(active.id, { objective: prompt, status: "active", tokenBudget: 50_000 });
  else await orchestrator.startTask(active.id);
  await waitFor(() => form?.taskId === active.id);
  // Inspect the actual native owners; do not replace their transports or close promises.
  for (const [taskId, adapter] of orchestrator.adapters) adapter.onExit(() => exits.push(taskId));
  const queued = await orchestrator.createTask("Must remain undispatched", agent, repo, "HEAD", { worktreeMode: "existing", worktreePath: repo });
  const startup = orchestrator.startTask(queued.id).then(() => "unexpected start", error => error.message);
  const input = handlers.get("orchestrator:iterate")({}, { taskId: idle.id, feedback: "Create never-sent.txt. Do not modify other files.", messageId: "quit-unsent" });
  await waitFor(() => queued.status === "queued" && idle.status === "queued");
  app.on("will-quit", event => { queueMicrotask(() => quitPrevented.push(event.defaultPrevented)); });
  registerAppShutdown(app, [async () => {
    await orchestrator.shutdown();
    const result = {
      agent, file: await readFile(path.join(repo, "before-shutdown.txt"), "utf8"),
      tasks: { idle: { id: idle.id, status: idle.status, feedback: idle.feedback }, active: { id: active.id, status: active.status, goal: active.goal?.status }, queued: { id: queued.id, status: queued.status } },
      exits, quitPrevented, input: await input, startup: await startup,
      snapshots: { idle: chat.snapshot(idle.id), active: chat.snapshot(active.id) }, events,
    };
    await writeFile(path.join(directory, "result.json"), JSON.stringify(result, null, 2));
    db.close();
  }]);
  app.quit();
  setTimeout(() => app.quit(), 0);
};
main().catch(async error => {
  await writeFile(path.join(directory, "failure.json"), JSON.stringify({ error: error.stack, events, tasks: orchestrator?.getAllTasks() }, null, 2));
  await orchestrator?.shutdown().catch(() => {});
  db?.close();
  app.exit(1);
});
