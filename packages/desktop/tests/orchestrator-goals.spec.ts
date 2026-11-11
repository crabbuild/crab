import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { mkdtemp, rm } from "node:fs/promises";
import { promisify } from "node:util";
import path from "node:path";
import os from "node:os";
import type { CodexGoal, CodexGoalMutation } from "../electron/orchestrator-adapters";

vi.mock("node:child_process", async original => ({ ...await original<typeof import("node:child_process")>(), spawn: vi.fn() }));
import { execFile, spawn } from "node:child_process";
import { Orchestrator, type Task } from "../electron/orchestrator";
import { OrchestratorTaskDatabase } from "../electron/orchestrator-task-db";

type Request = { id: number; method: string; params: Record<string, unknown> };
type Connection = { stdout: PassThrough; requests: Request[]; threadId?: string; turnId?: string; closed: boolean };
const run = promisify(execFile);

describe("native goal task ownership", () => {
  let cwd: string;
  let orchestrator: Orchestrator;
  let connections: Connection[];
  let goals: Map<string, CodexGoal>;
  let counter: number;
  let failResume: boolean;
  let holdTurn: boolean;
  let pendingTurn: { connection: Connection; id: number; result: unknown } | undefined;
  let holdGoalRead: boolean;
  let pendingGoalReads: Array<{ connection: Connection; id: number }>;
  const notify = (connection: Connection, method: string, params: object) => connection.stdout.write(JSON.stringify({ method, params }) + "\n");

  beforeEach(async () => {
    cwd = await mkdtemp(path.join(os.tmpdir(), "crab-goal-contract-"));
    await run("git", ["init", "--initial-branch=main", cwd]);
    await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd });
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(cwd, "settings.json"));
    connections = [];
    goals = new Map();
    counter = 0;
    failResume = false;
    holdTurn = false;
    pendingTurn = undefined;
    holdGoalRead = false;
    pendingGoalReads = [];
    const native = await vi.importActual<typeof import("node:child_process")>("node:child_process");
    vi.mocked(spawn).mockImplementation((...args) => {
      // Only the provider speaks the fixture protocol. Checkpoint Git must
      // execute normally so a fake close cannot stand in for its commit.
      if (args[0] !== "codex") return native.spawn(...args);
      const stdin = new PassThrough();
      const stdout = new PassThrough();
      const connection: Connection = { stdout, requests: [], closed: false };
      connections.push(connection);
      const child = Object.assign(new EventEmitter(), { stdin, stdout, stderr: new PassThrough(), kill: vi.fn(() => { close(); return true; }) });
      const close = () => { if (!connection.closed) { connection.closed = true; queueMicrotask(() => child.emit("close", 0)); } };
      stdin.on("finish", close);
      stdin.on("data", chunk => {
        for (const line of String(chunk).trim().split("\n")) {
          const request = JSON.parse(line) as Request;
          if (request.id === undefined || !request.method) continue;
          connection.requests.push(request);
          if (request.method === "thread/goal/get" && holdGoalRead) {
            pendingGoalReads.push({ connection, id: request.id });
            continue;
          }
          if (request.method === "thread/resume" && failResume) {
            stdout.write(JSON.stringify({ id: request.id, error: { code: -32000, message: "Owned resume failure" } }) + "\n");
            continue;
          }
          const params = request.params;
          const threadId = String(params.threadId ?? connection.threadId ?? "");
          let result: unknown = {};
          let notification: { method: string; params: object } | undefined;
          if (request.method === "thread/start" || request.method === "thread/resume") {
            connection.threadId = threadId || `thread-${++counter}`;
            result = { thread: { id: connection.threadId } };
            if (request.method === "thread/resume") {
              const goal = goals.get(threadId);
              notification = goal ? { method: "thread/goal/updated", params: { threadId, goal } } : { method: "thread/goal/cleared", params: { threadId } };
            }
          } else if (request.method === "turn/start") {
            connection.turnId = `turn-${++counter}`;
            result = { turn: { id: connection.turnId } };
          } else if (request.method === "turn/interrupt") {
            notification = { method: "turn/completed", params: { threadId, turn: { id: params.turnId, status: "interrupted" } } };
          } else if (request.method === "thread/goal/get") result = { goal: goals.get(threadId) ?? null };
          else if (request.method === "thread/goal/set") {
            const mutation = params as CodexGoalMutation;
            const goal: CodexGoal = { threadId, objective: "Owned goal", status: "active", tokenBudget: null, tokensUsed: 0, timeUsedSeconds: 0, createdAt: 1, updatedAt: 2, ...goals.get(threadId), ...mutation };
            if (goal.status === "active" && goal.tokenBudget !== null && goal.tokensUsed >= goal.tokenBudget) goal.status = "budgetLimited";
            goals.set(threadId, goal);
            result = { goal };
            notification = { method: "thread/goal/updated", params: { threadId, goal } };
          } else if (request.method === "thread/goal/clear") {
            result = { cleared: goals.delete(threadId) };
            notification = { method: "thread/goal/cleared", params: { threadId } };
          }
          if (request.method === "turn/start" && holdTurn) { pendingTurn = { connection, id: request.id, result }; continue; }
          stdout.write(JSON.stringify({ id: request.id, result }) + "\n");
          if (notification) notify(connection, notification.method, notification.params);
        }
      });
      return child as unknown as ReturnType<typeof spawn>;
    });
    orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), maxConcurrent: 1, timeoutMs: 60_000 });
  });

  afterEach(async () => {
    await orchestrator.shutdown();
    vi.unstubAllEnvs();
    await rm(cwd, { recursive: true, force: true });
  });

  const connectionFor = (task: Task) => {
    const connection = [...connections].reverse().find(client => client.threadId === task.codexThreadId && !client.closed);
    if (!connection) throw new Error("Owned provider connection missing");
    return connection;
  };
  const done = (task: Task) => {
    const connection = connectionFor(task);
    notify(connection, "turn/completed", { threadId: task.codexThreadId, turn: { id: connection.turnId, status: "completed" } });
  };
  const idleTask = async () => {
    const task = await orchestrator.createTask("Original prompt", "codex", cwd, "HEAD", { worktreeMode: "primary" });
    await orchestrator.startTask(task.id);
    done(task);
    await vi.waitFor(() => expect(task.status).toBe("reviewing"));
    return task;
  };

  it("holds capacity between goal turns and releases it only after the native goal stops", async () => {
    const task = await idleTask();
    const connection = connectionFor(task);
    await orchestrator.setGoal(task.id, { objective: "Finish owned work", status: "active", tokenBudget: 1000 });
    const other = await orchestrator.createTask("Other prompt", "codex", cwd, "HEAD", { worktreeMode: "primary" });
    const starting = orchestrator.startTask(other.id);
    await vi.waitFor(() => expect(other.status).toBe("queued"));
    notify(connection, "turn/started", { threadId: task.codexThreadId, turn: { id: "autonomous-turn" } });
    notify(connection, "turn/completed", { threadId: task.codexThreadId, turn: { id: "autonomous-turn", status: "completed" } });
    expect({ goalStatus: task.status, queued: other.status, ordinaryTurns: connection.requests.filter(request => request.method === "turn/start").length }).toEqual({ goalStatus: "running", queued: "queued", ordinaryTurns: 1 });
    const complete = { ...task.goal!, status: "complete" as const, tokensUsed: 150 };
    goals.set(task.codexThreadId!, complete);
    notify(connection, "thread/goal/updated", { threadId: task.codexThreadId, goal: complete });
    await starting;
    const db = new OrchestratorTaskDatabase(path.join(cwd, "tasks.sqlite"));
    try {
      expect({ original: task.prompt, feedback: task.feedback, goal: db.loadTasks().find(saved => saved.id === task.id)?.goal, status: other.status }).toEqual({ original: "Original prompt", feedback: ["Finish owned work"], goal: complete, status: "running" });
    } finally { db.close(); }
  });

  it("withdraws a queued goal before any native activation is sent", async () => {
    const task = await idleTask();
    const blocker = await orchestrator.createTask("Block capacity", "codex", cwd, "HEAD", { worktreeMode: "primary" });
    await orchestrator.startTask(blocker.id);
    const activation = orchestrator.setGoal(task.id, { objective: "Never dispatch", status: "active" }).catch(error => error);
    await vi.waitFor(() => expect(task.status).toBe("queued"));
    await orchestrator.clearGoal(task.id);
    expect({ error: (await activation).message, sets: connections.flatMap(client => client.requests).filter(request => request.method === "thread/goal/set"), goal: task.goal, feedback: task.feedback, blocker: blocker.status }).toEqual({
      error: "Goal activation cancelled", sets: [], goal: null, feedback: [], blocker: "running",
    });
  });

  it("pauses durable goals on Stop and reads or clears them cold without resuming inference", async () => {
    const task = await idleTask();
    await orchestrator.setGoal(task.id, { objective: "Durable goal", status: "active" });
    await orchestrator.cancelTask(task.id);
    const coldOffset = connections.length;
    const goal = await orchestrator.getGoal(task.id);
    await orchestrator.clearGoal(task.id);
    expect({ stopped: task.status, goal: goal?.status, current: task.goal,
      coldMethods: connections.slice(coldOffset).flatMap(client => client.requests).map(request => request.method),
      closed: connections.every(client => client.closed), pause: connections.flatMap(client => client.requests).reverse().find(request => request.method === "thread/goal/set")?.params,
    }).toEqual({ stopped: "stopped", goal: "paused", current: null, coldMethods: ["initialize", "thread/goal/get", "thread/goal/get", "initialize", "thread/goal/get", "thread/goal/clear"], closed: true,
      pause: { threadId: task.codexThreadId, status: "paused", origin: "automatic" },
    });
  });

  it("pauses an active goal on app shutdown without resuming its thread", async () => {
    const task = await idleTask();
    await orchestrator.setGoal(task.id, { objective: "Durable goal", status: "active" });
    const offset = connections.length;
    await orchestrator.shutdown();
    expect({ status: task.status, goal: goals.get(task.codexThreadId!)?.status, cached: task.goal?.status,
      closed: connections.every(client => client.closed), methods: connections.slice(offset).flatMap(client => client.requests).map(request => request.method),
    }).toEqual({ status: "stopped", goal: "paused", cached: "paused", closed: true,
      methods: ["initialize", "thread/goal/get", "thread/goal/get", "thread/goal/set"],
    });
  });

  it("joins a cancelled cold metadata read without reporting quit failure", async () => {
    const task = await idleTask();
    await orchestrator.cancelTask(task.id);
    holdGoalRead = true;
    const read = orchestrator.getGoal(task.id).catch(error => String(error));
    await vi.waitFor(() => expect(pendingGoalReads).toHaveLength(1));
    await orchestrator.shutdown();
    await read;
    expect(connections.every(client => client.closed)).toBe(true);
  });

  it("updates a cold goal before resume can continue its old objective, without resending a chat prompt", async () => {
    const task = await idleTask();
    await orchestrator.setGoal(task.id, { objective: "Old objective", status: "active" });
    await orchestrator.cancelTask(task.id);
    await orchestrator.setGoal(task.id, { objective: "Replacement objective", status: "active" });
    expect(connectionFor(task).requests.map(request => request.method)).toEqual(["initialize", "thread/goal/set", "thread/resume"]);
    expect(task.goal?.objective).toBe("Replacement objective");
  });

  it("retains uncertain activation but fails and pauses a goal when its cold resume rejects", async () => {
    const task = await idleTask();
    await orchestrator.cancelTask(task.id);
    failResume = true;
    await expect(orchestrator.setGoal(task.id, { objective: "Accepted before resume failed", status: "active" })).rejects.toThrow("delivery could not be confirmed");
    expect({ status: task.status, goal: task.goal?.status, feedback: task.feedback, closed: connections.every(client => client.closed) }).toEqual({
      status: "failed", goal: "paused", feedback: ["Accepted before resume failed"], closed: true,
    });
  });

  it("does not complete an old turn when an ordinary cold resume reports no goal", async () => {
    const task = await idleTask();
    await orchestrator.cancelTask(task.id);
    holdTurn = true;
    const statuses: string[] = [];
    orchestrator.on("orchestrator:task-status", payload => { if (payload.taskId === task.id) statuses.push(payload.status); });
    const submitting = orchestrator.iterate(task.id, "New ordinary input");
    await vi.waitFor(() => expect(pendingTurn).toBeDefined());
    expect({ status: task.status, oldCompletion: statuses.includes("reviewing") }).toEqual({ status: "running", oldCompletion: false });
    if (!pendingTurn) throw new Error("Native turn acknowledgement missing");
    pendingTurn.connection.stdout.write(JSON.stringify({ id: pendingTurn.id, result: pendingTurn.result }) + "\n");
    await submitting;
    done(task);
  });

  it("settles warm and cold activation that acknowledges an unchanged exhausted budget without starting a turn", async () => {
    const task = await idleTask();
    const connection = connectionFor(task);
    await orchestrator.setGoal(task.id, { objective: "Limited goal", status: "active", tokenBudget: 1 });
    notify(connection, "turn/started", { threadId: task.codexThreadId, turn: { id: "goal-turn" } });
    notify(connection, "turn/completed", { threadId: task.codexThreadId, turn: { id: "goal-turn", status: "completed" } });
    const limited = { ...task.goal!, status: "budgetLimited" as const, tokensUsed: 1 };
    goals.set(task.codexThreadId!, limited);
    notify(connection, "thread/goal/updated", { threadId: task.codexThreadId, goal: limited });
    await orchestrator.setGoal(task.id, { status: "active" });
    expect({ status: task.status, goal: task.goal?.status, ordinaryTurns: connection.requests.filter(request => request.method === "turn/start").length }).toEqual({ status: "reviewing", goal: "budgetLimited", ordinaryTurns: 1 });
    await orchestrator.cancelTask(task.id);
    await orchestrator.retryTask(task.id);
    expect({ status: task.status, goal: task.goal?.status, methods: connectionFor(task).requests.map(request => request.method) }).toEqual({ status: "reviewing", goal: "budgetLimited", methods: ["initialize", "thread/goal/set", "thread/resume"] });
  });

  it("lets a metadata read finish when a cold chat startup replaces only its provider owner", async () => {
    const task = await idleTask();
    await orchestrator.cancelTask(task.id);
    holdGoalRead = true;
    const reading = orchestrator.getGoal(task.id);
    await vi.waitFor(() => expect(pendingGoalReads).toHaveLength(1));
    await orchestrator.iterate(task.id, "Resume ordinary work");
    holdGoalRead = false;
    for (const pending of pendingGoalReads) pending.connection.stdout.write(JSON.stringify({ id: pending.id, result: { goal: null } }) + "\n");
    expect({ goal: await reading, readerClosed: pendingGoalReads[0].connection.closed, providerClosed: connectionFor(task).closed, status: task.status }).toEqual({ goal: null, readerClosed: true, providerClosed: false, status: "running" });
    done(task);
  });

  it("restores a lost goal owner as interrupted rather than treating its last completed cycle as goal completion", async () => {
    const task = await idleTask();
    await orchestrator.setGoal(task.id, { objective: "Keep working", status: "active" });
    const connection = connectionFor(task);
    notify(connection, "turn/started", { threadId: task.codexThreadId, turn: { id: "goal-cycle" } });
    notify(connection, "turn/completed", { threadId: task.codexThreadId, turn: { id: "goal-cycle", status: "completed" } });
    const count = connections.length;
    const restored = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite") }).getTask(task.id);
    expect({ status: restored?.status, goal: restored?.goal?.status, connections: connections.length }).toEqual({ status: "interrupted", goal: "active", connections: count });
  });
});
