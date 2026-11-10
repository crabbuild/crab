/**
 * Codex App-Server Adapter.
 *
 * Drives a persistent `codex app-server --listen stdio://` process over
 * JSON-RPC 2.0 (newline-delimited stdio). Replaces the per-turn
 * `codex exec --json` subprocess with a long-lived connection that
 * supports mid-turn steering, live approval responses, and thread resume.
 */

import { spawn } from "node:child_process";
import * as readline from "node:readline";
import { randomUUID } from "node:crypto";
import { StringDecoder } from "node:string_decoder";

import {
  AgentAdapter,
  getExpandedEnv,
  type AgentEvent,
  type AgentQuestionInput,
  type AgentInputResponse,
  type AgentTurnOptions,
  type AgentModel,
  type CodexGoal,
  type CodexGoalMutation,
} from "./orchestrator-adapters";
import { elicitationRequest, type PendingElicitation } from "./agent-elicitation";
import { AgentSubmissionUncertainError } from "./orchestrator-adapters";
import { CodexRpcClient, CodexRpcError, type CodexRequestId } from "./codex-rpc";
import {
  CODEX_TRACE_LINE_RE,
  CODEX_NOISE_LINE_RE,
  CODEX_MCP_NOISE_RE,
  stripAnsi,
  parseCodexEvents,
  extractUsageEvent,
  type CodexParseState,
} from "./codex-shared";

// ─── Constants ───────────────────────────────────────────────────────────────

/** Grace period to wait for turn/completed after sending turn/interrupt. */
const CANCEL_GRACE_MS = 5_000;

/** Time to wait after rpc.close() before sending SIGTERM. */
const CANCEL_SIGTERM_MS = 2_000;

/** Time to wait after SIGTERM before sending SIGKILL. */
const CANCEL_SIGKILL_MS = 5_000;

// ─── Internal Types ──────────────────────────────────────────────────────────

type CodexInitialAction =
  | { kind: "message"; content: string; options?: AgentTurnOptions }
  | { kind: "review"; content: string }
  | { kind: "goal"; mutation: CodexGoalMutation };

interface PendingApproval {
  /** The wire-level JSON-RPC request id we must respond to. */
  rpcId: CodexRequestId;
  /** Recorded for the approval-round-trip metric. */
  receivedAt: number;
  responses: Map<string, object>;
  cancellation: object;
}

interface ConnectionMetrics {
  initMs: number;
  turnCount: number;
  interruptCount: number;
  steerCount: number;
  approvalRoundTripMs: number[];
  reconnectCount: number;
  backpressureRejects: number;
}

// ─── Adapter ─────────────────────────────────────────────────────────────────

/**
 * Codex adapter that drives a persistent `codex app-server` process over
 * JSON-RPC 2.0 (newline-delimited stdio). One instance per task; the
 * connection survives across iterations so follow-ups use `turn/start`
 * on the existing thread rather than re-spawning.
 */
export class CodexAppServerAdapter extends AgentAdapter {
  readonly name = "Codex";
  readonly binary: string;

  /** Set by the orchestrator before spawn() when resuming a thread. */
  resumeThreadId: string | null = null;

  /** Extra CLI arguments passed from per-agent config. */
  extraArgs: string[];

  /** Notification methods to suppress on the connection (CA7.4). */
  private optOutNotifications: string[];

  // Private state
  private rpc: CodexRpcClient | null = null;
  private threadId: string | null = null;
  private activeTurnId: string | null = null;
  private pendingApprovals: Map<string, PendingApproval> = new Map();
  private pendingInputs = new Map<string, { rpcId: CodexRequestId; elicitation?: PendingElicitation }>();
  private initialization: Promise<void> | null = null;
  private metrics: ConnectionMetrics = {
    initMs: 0,
    turnCount: 0,
    interruptCount: 0,
    steerCount: 0,
    approvalRoundTripMs: [],
    reconnectCount: 0,
    backpressureRejects: 0,
  };

  /** Guards against multiple concurrent cancel() calls. */
  private cancelInProgress = false;
  private cancellation?: Promise<void>;

  /** Resolves when cancellation receives the terminal turn notification. */
  private turnCompletedResolve: (() => void) | null = null;

  private startingTurn: Promise<void> | null = null;
  private submission: Promise<void> = Promise.resolve();
  private model?: string;
  private effort: string | null = null;
  private mode: "plan" | "instant" = "instant";
  private cwd?: string;

  private lastUsageSnapshot = "";

  /** Tracks active command/exec sessions (sessionId → metadata). */
  private commandSessions = new Map<string, { stdout: StringDecoder; stderr: StringDecoder }>();
  private goalSnapshot?: CodexGoal | null;
  private goalRevision = 0;
  private reviewTurn: { id?: string; runtimeTurnId?: string; started: boolean; children: Set<string>; pending: Array<() => void> } | null = null;

  constructor(extraArgs?: string[], optOutNotifications?: string[], binary = "codex") {
    super();
    this.binary = binary;
    this.extraArgs = extraArgs ?? [];
    this.optOutNotifications = optOutNotifications ?? [];
  }

  // ─── AgentAdapter contract ─────────────────────────────────────────────────

  /**
   * Spawn one app-server connection and start the first turn.
   */
  spawn(prompt: string, cwd: string, env?: Record<string, string>, options?: AgentTurnOptions): Promise<void> {
    return this.doSpawn({ kind: "message", content: prompt, options }, cwd, env);
  }

  async listModels(cwd: string, env?: Record<string, string>): Promise<AgentModel[]> {
    if (!this.rpc) {
      if (this.cancelInProgress) throw new Error("Codex connection is closing");
      this.doSpawn(undefined, cwd, env);
    }
    await this.initialization;
    return this.readModels();
  }

  private async readModels(): Promise<AgentModel[]> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const models: AgentModel[] = [];
    let cursor: string | null = null;
    do {
      const page = await this.rpc.send("model/list", { cursor, includeHidden: false }, { timeoutMs: 0 }) as {
        data: Array<{ model: string; displayName: string; description: string; hidden: boolean; isDefault: boolean; defaultReasoningEffort: string; supportedReasoningEfforts: Array<{ reasoningEffort: string }> }>;
        nextCursor: string | null;
      };
      for (const model of page.data) {
        if (model.hidden) continue;
        models.push({ value: model.model, label: model.displayName, description: model.description,
          isDefault: model.isDefault, defaultIntelligence: model.defaultReasoningEffort,
          intelligenceLevels: model.supportedReasoningEfforts.map(effort => effort.reasoningEffort) });
      }
      cursor = page.nextCursor;
    } while (cursor);
    return models;
  }

  /** Append input to the active turn, or start a new turn after it completes. */
  iterate(prompt: string, options?: AgentTurnOptions): Promise<void> {
    // Submission acknowledgements serialize new-turn/steer decisions. Two
    // messages racing with completion must not both start a new turn.
    const request = this.submission.then(() => this.submit(prompt, options));
    this.submission = request.catch(() => {});
    return request;
  }

  private async submit(prompt: string, options?: AgentTurnOptions): Promise<void> {
    if (this.initialization) await this.initialization;
    if (this.reviewTurn) throw new Error("Wait for the review to finish before sending another message");
    if (this.startingTurn) await this.startingTurn;
    if (!this.rpc || this.cancelInProgress) throw new Error("No active Codex connection");
    const turnId = this.activeTurnId;
    if (!turnId) { await this.startTurn(prompt, options); return; }
    if (options?.model !== undefined || options?.reasoningLevel !== undefined || (options?.mode !== undefined && options.mode !== this.mode)) throw new Error("Wait for the current turn to complete before changing agent settings");
    this.metrics.steerCount++;
    try {
      await this.rpc.send("turn/steer", {
        threadId: this.threadId, expectedTurnId: turnId,
        input: [{ type: "text", text: prompt }],
      }, { timeoutMs: 0 });
    } catch (error: unknown) {
      // Only a definitive server rejection after observed completion can be
      // resubmitted as a new turn. A timeout may already have accepted input.
      if (error instanceof CodexRpcError && this.activeTurnId === null && !this.cancelInProgress) {
        await this.startTurn(prompt, options);
        return;
      }
      if (error instanceof CodexRpcError) throw error;
      throw new AgentSubmissionUncertainError(error);
    }
  }

  /**
   * Deliver the user's approval decision live over the JSON-RPC connection.
   * Maps Crab's decision vocabulary to the Codex protocol's and responds
   * to the original server-initiated request. No-op if the request has
   * already been resolved (e.g. renderer raced with shutdown).
   */
  respondApproval(requestId: string, decision: string): boolean {
    const pending = this.pendingApprovals.get(requestId);
    if (!pending) {
      // Renderer raced with shutdown — no-op is safe.
      return false;
    }
    if (!this.rpc || !pending.responses.has(decision)) return false;

    const response = pending.responses.get(decision);
    if (!response || !this.rpc.respond(pending.rpcId, response)) return false;
    this.metrics.approvalRoundTripMs.push(Date.now() - pending.receivedAt);
    this.pendingApprovals.delete(requestId);
    this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
    return true;
  }

  respondInput(requestId: string, response: AgentInputResponse): boolean {
    const pending = this.pendingInputs.get(requestId);
    if (!pending || !this.rpc) return false;
    let result: object;
    if (pending.elicitation) {
      if (response.kind !== "elicitation") return false;
      const answer = pending.elicitation.respond(response);
      result = { ...answer, content: answer.content ?? null };
    } else {
      if (response.kind !== "questions") return false;
      result = { answers: Object.fromEntries(Object.entries(response.answers).map(([id, answers]) => [id, { answers }])) };
    }
    if (!this.rpc.respond(pending.rpcId, result)) return false;
    this.pendingInputs.delete(requestId);
    this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
    return true;
  }

  /**
   * Gracefully shut down the adapter. Cancels pending approvals, interrupts
   * any active turn, closes the RPC transport,
   * then escalates through SIGTERM → SIGKILL if the process doesn't exit.
   * Idempotent — a second call while shutdown is in progress is a no-op.
   */
  cancel(): Promise<void> {
    if (this.cancelInProgress) return this.cancellation ?? Promise.resolve();
    this.cancelInProgress = true;
    const child = this.proc;
    this.cancellation = child ? new Promise(resolve => { child.once("close", () => resolve()); }) : Promise.resolve();

    void this.doCancel().catch(() => {
      child?.kill("SIGKILL");
    });
    return this.cancellation;
  }

  // ─── Private: spawn and initialization ─────────────────────────────────────

  private doSpawn(action: CodexInitialAction | undefined, cwd: string, env?: Record<string, string>): Promise<void> {
    if (this.proc || this.cancelInProgress) throw new Error("Codex is already connected or closing");
    this.cwd = cwd;
    this.exitEmitted = false;
    const args = ["app-server", "--listen", "stdio://", ...this.extraArgs];

    this.proc = spawn(this.binary, args, {
      cwd,
      env: getExpandedEnv(env),
      stdio: ["pipe", "pipe", "pipe"],
    });

    // Construct RPC client over stdin/stdout.
    this.rpc = new CodexRpcClient(this.proc.stdin!, this.proc.stdout!, {
      logger: (msg) => {
        if (process.env.CRAB_CODEX_DEBUG) {
          console.error(`[codex-rpc] ${msg}`);
        }
      },
    });

    // thread/started can precede the thread/start response. Register before
    // issuing requests so persistence never depends on notification timing.
    this.wireNotificationHandlers();
    this.wireServerRequestHandler();
    this.rpc.onClose(() => {
      this.clearRequests();
      this.activeTurnId = null;
      this.reviewTurn = null;
      this.turnCompletedResolve?.();
      if (!this.proc || this.cancelInProgress || this.exitEmitted) return;
      if (!this.doneEmitted || this.threadId && this.goalSnapshot?.status === "active") {
        this.emitEvent({ type: "error", timestamp: Date.now(), error: "Codex app-server connection closed before the turn completed" });
        this.emitEvent({ type: "done", timestamp: Date.now(), outcome: "failed", content: "" });
      }
      this.cancel();
    });

    // Optional: log every notification and emitted event when CRAB_CODEX_DEBUG is set.
    // This is opt-in (env var only) so it's safe to leave compiled in. Remove if it
    // ever becomes noise.
    if (process.env.CRAB_CODEX_DEBUG) {
      const originalEmit = this.emitEvent.bind(this);
      (this as any).emitEvent = (event: AgentEvent) => {
        try {
          const summary = JSON.stringify(event).slice(0, 500);
          console.error(`[codex-adapter] emit ${event.type} ${summary}`);
        } catch {
          console.error(`[codex-adapter] emit ${event.type} <unserialisable>`);
        }
        originalEmit(event);
      };
    }

    // Wire stderr through noise filters.
    this.wireStderr();

    // Drain stdout before closing the lifecycle; exit can precede buffered results.
    this.proc.on("close", (code) => {
      const exitCode = code ?? 1;
      const hadActiveTurn = this.activeTurnId !== null || this.startingTurn !== null;
      const hadActiveGoal = this.threadId !== null && this.goalSnapshot?.status === "active";
      this.proc = null;
      this.activeTurnId = null;
      this.reviewTurn = null;
      this.turnCompletedResolve?.();
      this.clearRequests();

      const incomplete = hadActiveTurn && !this.doneEmitted || hadActiveGoal && !this.cancelInProgress;
      if (incomplete) {
        if (!this.cancelInProgress) this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: "Codex app-server process exited mid-turn",
        });
        this.emitEvent({ type: "done", timestamp: Date.now(), outcome: this.cancelInProgress ? "interrupted" : "failed", content: "" });
      }

      this.rpc?.close();
      this.rpc = null;
      this.emitExit(incomplete ? exitCode || 1 : exitCode);
    });

    this.proc.on("error", (err: NodeJS.ErrnoException) => {
      if (this.cancelInProgress || this.exitEmitted) return;
      let message = err.message;
      if (err.code === "ENOENT") {
        message = `Agent binary "${this.binary}" not found on PATH. Please install it or configure the path in Settings → Agents.`;
      }
      this.emitEvent({
        type: "error",
        timestamp: Date.now(),
        error: message,
      });
      this.emitEvent({ type: "done", timestamp: Date.now(), outcome: "failed", content: "" });
      this.cancel();
    });

    // Run the initialization handshake.
    this.initialization = this.initializeAndStart(action).finally(() => { this.initialization = null; });
    void this.initialization.catch(() => {});
    return this.initialization;
  }

  private async initializeAndStart(action: CodexInitialAction | undefined): Promise<void> {
    if (!this.rpc) throw new Error("Codex connection closed before startup");
    const initStart = Date.now();
    let operation = "initialization";
    try {
      const capabilities: Record<string, unknown> = { experimentalApi: true };
      if (this.optOutNotifications.length) capabilities.optOutNotificationMethods = this.optOutNotifications;
      await this.rpc.send("initialize", {
        clientInfo: { name: "crab_desktop", title: "Crab Desktop", version: "0.1.0" }, capabilities,
      }, action ? { timeoutMs: 0 } : undefined);
      if (this.cancelInProgress) throw new Error("Codex startup was cancelled");
      this.rpc.notify("initialized");
      this.metrics.initMs = Date.now() - initStart;
      if (!action) return;
      if (action.kind === "goal" && this.resumeThreadId) {
        // Resuming can continue a saved active goal immediately. Update durable
        // goal metadata first so that continuation sees the requested objective.
        operation = "thread/goal/set";
        await this.setGoal(action.mutation, "user", this.resumeThreadId);
        operation = "thread resume";
        try { await this.startOrResumeThread(); } catch (error: unknown) {
          throw new AgentSubmissionUncertainError(error);
        }
        return;
      }
      operation = "thread start";
      await this.startOrResumeThread();
      if (this.cancelInProgress) throw new Error("Codex startup was cancelled");
      operation = action.kind === "goal" ? "thread/goal/set" : action.kind === "review" ? "review/start" : "turn/start";
      if (action.kind === "goal") await this.setGoal(action.mutation);
      else if (action.kind === "review") await this.startReview(action.content);
      else await this.startTurn(action.content, action.options);
    } catch (error: unknown) {
      if (!this.cancelInProgress && !this.exitEmitted) {
        const message = error instanceof Error ? error.message : String(error);
        this.emitEvent({ type: "error", timestamp: Date.now(), error: `Codex ${operation} failed: ${message}` });
        this.emitEvent({ type: "done", timestamp: Date.now(), outcome: "failed", content: "" });
        this.cancel();
      }
      throw error;
    }
  }

  private async startOrResumeThread(): Promise<void> {
    if (!this.rpc) return;

    const method = this.resumeThreadId ? "thread/resume" : "thread/start";
    // Resume otherwise restores the old workspace from history. An owned
    // worktree may have been recreated at a different path since that turn.
    const params = { cwd: this.cwd, ...(this.resumeThreadId ? { threadId: this.resumeThreadId } : {}) };
    // Resume must load the stored thread before turn/start. A failure must
    // surface to the user; starting a fresh thread would silently lose context.
    const result = await this.rpc.send(method, params, { timeoutMs: 0 }) as { thread?: { id?: unknown }; model?: string; reasoningEffort?: string | null; collaborationMode?: { mode: string } | null } | null;
    const threadId = result?.thread?.id;
    if (typeof threadId !== "string" || !threadId) {
      throw new Error(`${method} returned no thread.id`);
    }
    this.model = result?.model;
    this.effort = result?.reasoningEffort ?? null;
    this.mode = result?.collaborationMode?.mode === "plan" ? "plan" : "instant";
    if (this.threadId !== threadId) {
      this.threadId = threadId;
      this.emitEvent({ type: "thread_started", timestamp: Date.now(), threadId });
    }
    this.resumeThreadId = null;
  }

  // ─── Private: turn management ──────────────────────────────────────────────

  private async startTurn(prompt: string, options?: AgentTurnOptions): Promise<void> {
    if (!this.rpc || this.cancelInProgress) return Promise.reject(new Error("No active Codex connection"));
    let model = options?.model === null ? undefined : options?.model ?? this.model;
    let effort = options?.reasoningLevel === undefined ? this.effort : options.reasoningLevel;
    if (options?.model === null || options?.reasoningLevel === null) {
      // A resumed thread reports its saved overrides. Resolve resets from
      // effective CLI config/catalog so clearing a picker does not reuse them.
      const { config } = await this.rpc.send("config/read", { cwd: this.cwd, includeLayers: false }, { timeoutMs: 0 }) as { config: { model?: string | null; model_reasoning_effort?: string | null } };
      const models = await this.readModels();
      if (options.model === null) model = config.model ?? models.find(model => model.isDefault)?.value;
      if (options.reasoningLevel === null) effort = config.model_reasoning_effort ?? models.find(entry => entry.value === model)?.defaultIntelligence ?? null;
    }
    const mode = options?.mode ?? this.mode;
    const needsMode = options?.mode !== undefined || mode === "plan" || options?.reasoningLevel === null;
    if (needsMode && !model) throw new Error("Codex returned no effective model for collaboration mode");
    if (!this.rpc || this.cancelInProgress) throw new Error("No active Codex connection");
    this.doneEmitted = false;
    this.metrics.turnCount++;
    const request = this.rpc.send("turn/start", {
      threadId: this.threadId,
      input: [{ type: "text", text: prompt }],
      ...(needsMode ? { collaborationMode: { mode: mode === "plan" ? "plan" : "default", settings: { model, reasoning_effort: effort, developer_instructions: null } } }
        : { ...(options?.model !== undefined ? { model } : {}), ...(options?.reasoningLevel !== undefined ? { effort } : {}) }),
    }, { timeoutMs: 0 }).then(result => {
      const turn = (result as { turn?: { id?: string } })?.turn;
      if (!turn?.id) throw new Error("turn/start returned no turn.id");
      this.model = model;
      this.effort = effort;
      this.mode = mode;
      if (options) this.emitEvent({ type: "configuration_changed", timestamp: Date.now(), turnId: turn.id, configuration: options });
      if (!this.doneEmitted) this.activeTurnId = turn.id;
    }).catch((error: unknown) => {
      if (error instanceof CodexRpcError) throw error;
      throw new AgentSubmissionUncertainError(error);
    }).finally(() => { this.startingTurn = null; });
    this.startingTurn = request;
    return request;
  }

  private waitForTurnCompleted(timeoutMs: number): Promise<void> {
    return new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.turnCompletedResolve = null;
        resolve();
      }, timeoutMs);

      this.turnCompletedResolve = () => {
        clearTimeout(timer);
        this.turnCompletedResolve = null;
        resolve();
      };
    });
  }

  // ─── Private: notification and server-request wiring ───────────────────────

  /** Wire lifecycle and item notifications before creating the thread. */
  private wireNotificationHandlers(): void {
    if (!this.rpc) return;

    const state: CodexParseState = { agentMessages: new Map(), lastAgentMessage: "" };
    const onThreadNotification = (method: string, handler: (params: unknown) => void) => {
      const receive = (params: unknown) => {
        const p = params as Record<string, unknown> | undefined;
        if (p?.threadId !== (this.threadId ?? this.resumeThreadId)) return;
        const review = this.reviewTurn;
        if (!review) { handler(params); return; }
        // Inline review forwards its delegate's start/items, but completes the
        // parent review turn. Keep one client turn, including events before ack.
        const item = p.item as { type?: string } | undefined;
        if (!review.id && item?.type === "enteredReviewMode" && typeof p.turnId === "string") this.bindReviewTurn(review, p.turnId);
        if (!review.id) { review.pending.push(() => receive(params)); return; }
        const turn = p.turn as { id?: string } | undefined;
        let normalized = p;
        if (method === "turn/started" && turn?.id) {
          review.children.add(turn.id);
          review.runtimeTurnId = turn.id;
          if (review.started) return;
          review.started = true;
          normalized = { ...p, turn: { ...turn, id: review.id } };
        } else if (method === "turn/completed" && turn?.id !== review.id) return;
        if (typeof p.turnId === "string" && review.children.has(p.turnId)) normalized = { ...normalized, turnId: review.id };
        handler(normalized);
      };
      this.rpc?.onNotification(method, receive);
    };

    // Thread lifecycle
    this.rpc.onNotification("thread/started", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const thread = p?.thread as Record<string, unknown> | undefined;
      const threadId = String(thread?.id ?? p?.threadId ?? "");
      if (threadId && this.threadId === null && (!this.resumeThreadId || threadId === this.resumeThreadId)) {
        this.threadId = threadId;
        this.emitEvent({
          type: "thread_started",
          timestamp: Date.now(),
          threadId,
        });
      }
    });

    onThreadNotification("thread/goal/updated", (params: unknown) => {
      const { goal } = params as { goal: CodexGoal };
      this.publishGoal(goal);
    });
    onThreadNotification("thread/goal/cleared", () => {
      this.publishGoal(null);
    });

    // Turn lifecycle
    onThreadNotification("turn/started", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const turn = p?.turn as Record<string, unknown> | undefined;
      const turnId = String(turn?.id ?? p?.turnId ?? "");
      if (turnId) {
        this.activeTurnId = turnId;
      }
      this.lastUsageSnapshot = "";
      this.doneEmitted = false;
      state.lastAgentMessage = "";
      state.agentMessages.clear();
      this.emitEvent({ type: "turn_started", timestamp: Date.now(), turnId });
    });

    onThreadNotification("turn/completed", (params: unknown) => {
      const p = params as { threadId: string; turn: { id: string; status: string; error?: { message?: string } } };
      if (p.turn.id !== this.activeTurnId || this.doneEmitted) return;
      this.activeTurnId = null;
      this.reviewTurn = null;
      this.turnCompletedResolve?.();
      // Server-request resolution is authoritative. Codex also cancels requests
      // on turn transitions; do not race that notification with local cleanup.
      const status = p.turn.status;
      const outcome = status === "completed" || status === "interrupted" ? status : "failed";
      if (outcome === "failed" && !this.cancelInProgress) {
        this.emitEvent({ type: "error", timestamp: Date.now(), error: p.turn.error?.message ?? "Codex turn failed" });
      }
      const content = outcome === "completed" ? state.lastAgentMessage : "";
      this.emitEvent({ type: "done", timestamp: Date.now(), turnId: p.turn.id, outcome, content });
      state.lastAgentMessage = "";
    });

    onThreadNotification("item/agentMessage/delta", (params: unknown) => {
      const p = params as Record<string, unknown>;
      if (p.turnId !== this.activeTurnId) return;
      for (const event of parseCodexEvents({ method: "item/agentMessage/delta", params: p }, state)) this.emitEvent(event);
    });

    onThreadNotification("item/reasoning/summaryTextDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.emitEvent({ type: "thinking", timestamp: Date.now(), content: delta });
      }
    });

    onThreadNotification("item/reasoning/textDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.emitEvent({ type: "thinking", timestamp: Date.now(), content: delta });
      }
    });

    onThreadNotification("item/plan/delta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.emitEvent({ type: "thinking", timestamp: Date.now(), content: delta });
      }
    });

    // Terminal interaction updates during command execution — informational,
    // no event emission needed. Silently acknowledge to suppress the
    // "Unknown notification method" warning.
    onThreadNotification("item/commandExecution/terminalInteraction", () => {
      // Intentionally empty — terminal interaction data is not surfaced
      // in the chat UI. The command output arrives via outputDelta.
    });

    onThreadNotification("item/commandExecution/outputDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? p?.output ?? p?.content ?? p?.text ?? "");
      if (delta) {
        this.emitEvent({
          type: "tool_result",
          timestamp: Date.now(),
          tool: "Bash",
          itemId: p?.itemId ? String(p.itemId) : undefined,
          output: delta,
        });
      }
    });

    onThreadNotification("item/fileChange/outputDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? p?.output ?? p?.content ?? p?.text ?? "");
      if (delta) {
        this.emitEvent({
          type: "tool_result",
          timestamp: Date.now(),
          tool: "Write",
          itemId: p?.itemId ? String(p.itemId) : undefined,
          output: delta,
        });
      }
    });

    onThreadNotification("item/completed", (params: unknown) => {
      // Route through parseCodexEvents for full item parsing.
      const p = params as Record<string, unknown> | undefined;
      if (!p || p.turnId !== this.activeTurnId) return;
      const raw: Record<string, unknown> = { method: "item/completed", params: p };
      const events = parseCodexEvents(raw, state);
      for (const event of events) {
        this.emitEvent(event);
      }
    });

    onThreadNotification("item/started", (params: unknown) => {
      // Route through parseCodexEvents to emit tool_use events so the
      // renderer can show tool cards (Bash, Write, etc.) when items begin.
      const p = params as Record<string, unknown> | undefined;
      if (!p || p.turnId !== this.activeTurnId) return;
      const raw: Record<string, unknown> = { method: "item/started", params: p };
      const events = parseCodexEvents(raw, state);
      for (const event of events) {
        this.emitEvent(event);
      }
    });

    // Mid-turn error notifications (D11).
    onThreadNotification("error", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const willRetry = p?.willRetry === true;
      const errorObj = p?.error as Record<string, unknown> | undefined;
      const message = String(errorObj?.message ?? p?.message ?? "Codex reported an error");
      const code = errorObj?.code;

      if (willRetry) {
        this.emitEvent({
          type: "generic",
          timestamp: Date.now(),
          tool: "ServerWarning",
          input: { error: message, willRetry: true, code },
        });
      } else {
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: message,
          input: { code },
        });
      }
    });

    // turn/plan/updated
    onThreadNotification("turn/plan/updated", (params: unknown) => {
      this.emitEvent({
        type: "task",
        timestamp: Date.now(),
        tool: "EnterPlanMode",
        input: params,
      });
    });

    // model/rerouted
    onThreadNotification("model/rerouted", (params: unknown) => {
      this.emitEvent({
        type: "generic",
        timestamp: Date.now(),
        tool: "ModelRerouted",
        input: params,
      });
    });

    // serverRequest/resolved — internal bookkeeping.
    this.rpc.onNotification("serverRequest/resolved", (params: unknown) => {
      // Clean up any lingering pending approval if still present.
      const p = params as Record<string, unknown> | undefined;
      const requestId = JSON.stringify(p?.requestId);
      if (requestId) {
        const approval = this.pendingApprovals.delete(requestId);
        const input = this.pendingInputs.delete(requestId);
        if (approval || input) this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
      }
    });

    // thread/closed — trigger graceful shutdown.
    onThreadNotification("thread/closed", () => {
      this.cancel();
    });

    // Side-effect-only notifications (drop).
    this.rpc.onNotification("turn/diff/updated", () => {});
    this.rpc.onNotification("thread/status/changed", () => {});
    this.rpc.onNotification("item/reasoning/summaryPartAdded", () => {});
    this.rpc.onNotification("item/mcpToolCall/progress", () => {});
    // Codex sends these but we don't need to act on them.
    this.rpc.onNotification("mcpServer/startupStatus/updated", () => {});
    this.rpc.onNotification("hook/started", () => {});
    this.rpc.onNotification("hook/completed", () => {});
    this.rpc.onNotification("account/rateLimits/updated", () => {});
    onThreadNotification("thread/tokenUsage/updated", (params: unknown) => {
      const p = params as { turnId: string; tokenUsage: { last: Record<string, unknown>; total: Record<string, unknown> } };
      // Resume replays historical usage; last is one API response, not a turn.
      // Total changes identify new responses, including identical-sized calls.
      if (p.turnId !== this.activeTurnId) return;
      const total = p.tokenUsage.total;
      const snapshot = JSON.stringify([total.inputTokens, total.cachedInputTokens, total.cacheWriteInputTokens,
        total.outputTokens, total.reasoningOutputTokens, total.totalTokens]);
      if (snapshot === this.lastUsageSnapshot) return;
      this.lastUsageSnapshot = snapshot;
      const event = extractUsageEvent(p.tokenUsage.last);
      if (event) this.emitEvent({ ...event, turnId: p.turnId });
    });

    // command/exec/outputDelta — stream command output by session
    this.rpc.onNotification("command/exec/outputDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const sessionId = String(p?.processId ?? "");
      const session = this.commandSessions.get(sessionId);
      if (!session || typeof p?.deltaBase64 !== "string") return;
      const decoder = p.stream === "stderr" ? session.stderr : session.stdout;
      const delta = decoder.write(Buffer.from(p.deltaBase64, "base64"));
      this.emitEvent({
        type: "command_output",
        tool: "CommandSession",
        timestamp: Date.now(),
        commandSessionId: sessionId,
        content: delta,
      });
    });

    // fs/changed — relay filesystem watch events
    this.rpc.onNotification("fs/changed", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const watchId = String(p?.watchId ?? "");
      const paths = Array.isArray(p?.changedPaths) ? p.changedPaths : [];
      for (const path of paths) {
        this.emitEvent({
          type: "filesystem_change",
          tool: "FileWatchNotification",
          timestamp: Date.now(),
          path: String(path),
          fsWatchId: watchId,
        });
      }
    });

    // Unknown notification methods: log debug and drop (CA7.3).
    this.rpc.onUnhandledNotification((method: string) => {
      console.debug(`[CodexAppServerAdapter] Unknown notification method: ${method}`);
    });
  }

  /** Route provider requests to the corresponding pending user decision. */
  private wireServerRequestHandler(): void {
    if (!this.rpc) return;

    this.rpc.onServerRequest((id: CodexRequestId, method: string, params: unknown) => {
      const pendingId = JSON.stringify(id);
      if (this.pendingInputs.has(pendingId) || this.pendingApprovals.has(pendingId)) return;
      if (this.cancelInProgress) {
        this.rpc?.respondError(id, -32000, "Conversation stopped");
        return;
      }
      if (method === "mcpServer/elicitation/request") {
        const p = params as { serverName: string; message: string; mode?: string; requestedSchema?: unknown; url?: string };
        try {
          const requestId = JSON.stringify(id);
          const request = elicitationRequest({ requestId, serverName: p.serverName, message: p.message, mode: p.mode, schema: p.requestedSchema, url: p.url });
          this.pendingInputs.set(requestId, { rpcId: id, elicitation: request });
          this.emitEvent({ type: "input_request", timestamp: Date.now(), inputRequest: request.request });
        } catch (error: unknown) {
          const message = error instanceof Error ? error.message : "Invalid MCP request";
          this.rpc?.respondError(id, -32602, message);
          this.emitEvent({ type: "generic", timestamp: Date.now(), tool: "ServerWarning", input: { serverName: p.serverName, error: message } });
        }
        return;
      }
      if (method === "item/tool/requestUserInput") {
        const p = params as { itemId: string; questions: Array<{ id: string; question: string; header: string; options?: AgentQuestionInput["questions"][number]["options"]; isOther?: boolean; isSecret?: boolean }> };
        const requestId = JSON.stringify(id);
        this.pendingInputs.set(requestId, { rpcId: id });
        const questions = p.questions.map(question => ({ ...question, allowFreeform: question.isOther === true || !question.options?.length }));
        this.emitEvent({ type: "input_request", timestamp: Date.now(), itemId: p.itemId, inputRequest: { requestId, questions } });
        return;
      }
      if (method === "item/permissions/requestApproval") {
        const p = params as { permissions: object; cwd: string; reason?: string; itemId: string };
        const requestId = JSON.stringify(id);
        const cancellation = { permissions: {}, scope: "turn" };
        this.pendingApprovals.set(requestId, { rpcId: id, receivedAt: Date.now(), cancellation,
          responses: new Map([
            ["allow_once", { permissions: p.permissions, scope: "turn" }],
            ["allow", { permissions: p.permissions, scope: "session" }],
            ["deny", cancellation],
          ]),
        });
        this.emitEvent({ type: "approval_request", timestamp: Date.now(), itemId: p.itemId, approval: {
          requestId, kind: "other", summary: p.reason || "Grant additional permissions",
          detail: `${p.cwd}\n\n${JSON.stringify(p.permissions, null, 2)}`, blocking: true,
          options: [{ id: "allow_once", label: "Allow for this turn" }, { id: "allow", label: "Allow for session" }, { id: "deny", label: "Deny" }],
        } });
        return;
      }
      // Handle approval requests.
      if (
        method === "item/commandExecution/requestApproval" ||
        method === "item/fileChange/requestApproval"
      ) {
      const p = params as Record<string, unknown> | undefined;
        const kind: NonNullable<AgentEvent["approval"]>["kind"] =
          method === "item/fileChange/requestApproval" ? "file" : "command";

        // The JSON-RPC id identifies this request, including concurrent
        // approvals for the same item. Timestamps can collide.
        const requestId = JSON.stringify(id);
        const cmd = String(p?.command ?? "");
        const filePath = String(p?.grantRoot ?? "");
        const reason = String(p?.reason ?? "");

        const summary = kind === "command"
          ? (cmd ? `Run command: ${cmd}` : "Run command outside sandbox")
          : (filePath ? `Write outside sandbox: ${filePath}` : "Write file outside sandbox");

        // Build detail: combine the specific info (command/path) with the
        // protocol's reason string so it's always surfaced (CA5.5).
        const specific = kind === "command" ? cmd : filePath;
        const detail = specific && reason
          ? `${specific}\n\nReason: ${reason}`
          : (reason || specific);

        const choices = Array.isArray(p?.availableDecisions) ? p.availableDecisions : ["accept", "acceptForSession", "decline", "cancel"];
        const decisions = new Map<string, unknown>();
        const options: Array<{ id: string; label: string }> = [];
        for (const [index, choice] of choices.entries()) {
          let id: string;
          let label: string;
          if (choice === "accept") { id = "allow_once"; label = "Allow once"; }
          else if (choice === "acceptForSession") { id = "allow"; label = "Allow for session"; }
          else if (choice === "decline") { id = "deny"; label = "Deny"; }
          else if (choice === "cancel") { id = "cancel"; label = "Cancel turn"; }
          else if (choice && typeof choice === "object" && "acceptWithExecpolicyAmendment" in choice) {
            id = `execpolicy:${index}`;
            const amendment = choice.acceptWithExecpolicyAmendment as { execpolicy_amendment: string[] };
            label = `Allow command rule: ${amendment.execpolicy_amendment.join(" ")}`;
          } else if (choice && typeof choice === "object" && "applyNetworkPolicyAmendment" in choice) {
            id = `network:${index}`;
            const amendment = (choice.applyNetworkPolicyAmendment as { network_policy_amendment: { action: string; host: string } }).network_policy_amendment;
            label = `${amendment.action === "allow" ? "Allow" : "Block"} network access to ${amendment.host}`;
          }
          else continue;
          decisions.set(id, choice);
          options.push({ id, label });
        }

        // Store the pending approval for later response.
        this.pendingApprovals.set(requestId, {
          rpcId: id,
          receivedAt: Date.now(),
          responses: new Map([...decisions].map(([id, decision]) => [id, { decision }])),
          cancellation: { decision: "cancel" },
        });

        // Emit the approval_request event (no rpc id on the public event — D12).
        this.emitEvent({
          type: "approval_request",
          timestamp: Date.now(),
          approval: {
            requestId,
            kind,
            summary,
            detail: detail || undefined,
            blocking: true,
            options,
          },
        });
        return;
      }

      // Unknown server request — respond with method-not-found to avoid wedging.
      if (this.rpc) {
        this.rpc.respondError(id, -32601, "method not handled");
      }
    });
  }

  // ─── Private: stderr wiring ────────────────────────────────────────────────

  private wireStderr(): void {
    if (!this.proc?.stderr) return;

    const rl = readline.createInterface({ input: this.proc.stderr });
    rl.on("line", (line: string) => {
      const trimmed = line.trim();
      if (!trimmed) return;

      // Strip leading ANSI / CSI escape sequences before applying line-shape
      // matchers. Codex's `tracing` formatter colourises its level/target
      // tokens (and some terminal-aware tools eat the ESC byte but leave
      // the bare `[2m` / `[0m` markers behind), so the timestamp doesn't
      // necessarily appear at column 0.
      const cleaned = stripAnsi(trimmed);

      // Drop tracing-format lines (timestamp + level + target). These are
      // diagnostic, not user-actionable; the underlying error already
      // reaches us as a structured event on the JSON-RPC channel when it
      // matters.
      if (CODEX_TRACE_LINE_RE.test(cleaned)) return;

      // Drop OAuth-protected MCP worker chatter (Sentry, Linear, Neon, …).
      // These fire on every startup when the user hasn't auth'd; they're
      // noise in the chat surface and belong in the MCP settings UI.
      if (CODEX_MCP_NOISE_RE.test(cleaned)) return;

      // Drop the legacy "reading from stdin" status lines.
      if (CODEX_NOISE_LINE_RE.test(cleaned)) return;

      // Anything that survives is unexpected stderr from a process that's
      // *supposed* to communicate over JSON-RPC. The bar for surfacing a
      // red error bubble in the chat is high — log at debug instead and
      // let the actual error path (turn/failed, error notification, exit
      // with non-zero status) drive the user-visible error.
      if (process.env.CRAB_CODEX_DEBUG) {
        console.debug(`[codex-adapter] stderr: ${cleaned}`);
      }
    });
  }

  // ─── Private: graceful shutdown ────────────────────────────────────────────

  private async doCancel(): Promise<void> {
    // Step 1: Cancel all pending approvals.
    for (const [, pending] of this.pendingApprovals) {
      if (this.rpc) {
        this.rpc.respond(pending.rpcId, pending.cancellation);
      }
    }
    for (const pending of this.pendingInputs.values()) this.rpc?.respond(pending.rpcId, pending.elicitation ? { action: "cancel", content: null } : { answers: {} });
    this.clearRequests();

    if (this.startingTurn) {
      await Promise.race([this.startingTurn.catch(() => {}), this.delay(CANCEL_GRACE_MS)]);
    }
    if (this.activeTurnId !== null && this.rpc) {
      const completed = this.waitForTurnCompleted(CANCEL_GRACE_MS);
      this.metrics.interruptCount++;
      const turnId = this.reviewTurn?.runtimeTurnId ?? this.activeTurnId;
      void this.rpc.send("turn/interrupt", {
        threadId: this.threadId ?? this.resumeThreadId, turnId,
      }).catch(() => {});
      await completed;
    }

    // Step 4: Close the RPC transport.
    if (this.rpc) {
      this.rpc.close();
      this.rpc = null;
    }

    // Step 5: Wait CANCEL_SIGTERM_MS, then SIGTERM if still alive.
    if (this.proc) {
      await this.delay(CANCEL_SIGTERM_MS);
      if (this.proc) {
        this.proc.kill("SIGTERM");

        // Step 6: Wait CANCEL_SIGKILL_MS, then SIGKILL if still alive.
        await this.delay(CANCEL_SIGKILL_MS);
        if (this.proc) {
          this.proc.kill("SIGKILL");
        }
      }
    }
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  // ─── Thread Operations ─────────────────────────────────────────────────

  /**
   * Fork the current thread into a new, independent thread.
   * Returns the new thread id so the renderer can start a new task.
   */
  async forkThread(): Promise<{ threadId: string }> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("thread/fork", {
      threadId: this.threadId,
    }) as Record<string, unknown> | undefined;
    const thread = (result?.thread ?? result) as Record<string, unknown> | undefined;
    const newId = String(thread?.id ?? result?.id ?? "");
    if (!newId) throw new Error("thread/fork did not return a thread id");
    return { threadId: newId };
  }

  /** Start context compaction on the current thread. */
  async compactThread(): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/compact/start", {
      threadId: this.threadId,
    });
    return {};
  }

  /** Set a human-readable name for the current thread. */
  async setThreadName(name: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/name/set", {
      threadId: this.threadId,
      name,
    });
    return {};
  }

  /** Set a goal for the current thread. */
  async setGoal(mutation: CodexGoalMutation, origin: "user" | "automatic" = "user", threadId: string | null = this.threadId): Promise<CodexGoal> {
    if (!this.rpc) throw new Error("No active Codex connection");
    if (!threadId) throw new Error("No Codex thread for this goal");
    const revision = this.goalRevision;
    try {
      const result = await this.rpc.send("thread/goal/set", { threadId, ...mutation, origin }) as { goal: CodexGoal };
      if (revision === this.goalRevision) this.publishGoal(result.goal);
      return this.goalSnapshot ?? result.goal;
    } catch (error: unknown) {
      if (error instanceof CodexRpcError) throw error;
      throw new AgentSubmissionUncertainError(error);
    }
  }

  /** Get the current thread's goal. */
  async getGoal(threadId?: string): Promise<CodexGoal | null> {
    if (this.initialization) await this.initialization;
    if (!this.rpc) throw new Error("No active Codex connection");
    const target = threadId ?? this.threadId;
    if (!target) throw new Error("No Codex thread for this goal");
    const revision = this.goalRevision;
    const result = await this.rpc.send("thread/goal/get", {
      threadId: target,
    }) as { goal: CodexGoal | null };
    if (revision === this.goalRevision) this.publishGoal(result.goal);
    return this.goalSnapshot ?? null;
  }

  /** Clear the current thread's goal. */
  async clearGoal(threadId: string | null = this.threadId): Promise<void> {
    if (!this.rpc) throw new Error("No active Codex connection");
    if (!threadId) throw new Error("No Codex thread for this goal");
    const revision = this.goalRevision;
    try {
      await this.rpc.send("thread/goal/clear", { threadId, origin: "user" });
      if (revision === this.goalRevision) this.publishGoal(null);
    } catch (error: unknown) {
      if (error instanceof CodexRpcError) throw error;
      throw new AgentSubmissionUncertainError(error);
    }
  }

  async readGoal(threadId: string, cwd: string, env?: Record<string, string>): Promise<CodexGoal | null> {
    if (!this.rpc) await this.doSpawn(undefined, cwd, env);
    if (this.initialization) await this.initialization;
    return this.getGoal(threadId);
  }

  async startGoal(mutation: CodexGoalMutation, cwd: string, env?: Record<string, string>): Promise<void> {
    await this.doSpawn({ kind: "goal", mutation }, cwd, env);
  }

  get hasActiveTurn(): boolean { return this.activeTurnId !== null; }

  private publishGoal(goal: CodexGoal | null): void {
    if (JSON.stringify(goal) === JSON.stringify(this.goalSnapshot)) return;
    this.goalSnapshot = goal;
    this.goalRevision++;
    this.emitEvent({ type: "goal_updated", timestamp: Date.now(), goal });
  }

  /** Update arbitrary metadata on the current thread. */
  async updateThreadMetadata(metadata: Record<string, unknown>): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/metadata/update", {
      ...metadata,
      threadId: this.threadId,
    });
    return {};
  }

  /**
   * Run a shell command outside the sandbox on the current thread.
   * The command is executed by the app-server, not the agent.
   */
  async shellCommand(command: string, cwd?: string): Promise<{ result: unknown }> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const params: Record<string, unknown> = {
      threadId: this.threadId,
      command,
    };
    if (cwd) params.cwd = cwd;
    const result = await this.rpc.send("thread/shellCommand", params);
    return { result };
  }

  /** Clean up background terminal sessions on the current thread. */
  async cleanBackgroundTerminals(): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/backgroundTerminals/clean", {
      threadId: this.threadId,
    });
    return {};
  }

  /**
   * Inject model-visible items into the current thread's history.
   * This is an experimental protocol feature.
   */
  async injectItems(items: Array<{ role: string; content: unknown }>): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/inject_items", {
      threadId: this.threadId,
      items,
    });
    return {};
  }

  // ─── Review ────────────────────────────────────────────────────────────

  async review(instructions: string, cwd: string, env?: Record<string, string>, _options?: AgentTurnOptions): Promise<void> {
    if (!this.proc) {
      await this.doSpawn({ kind: "review", content: instructions }, cwd, env);
      return;
    }
    const request = this.submission.then(async () => {
      if (this.initialization) await this.initialization;
      if (this.activeTurnId) throw new Error("Wait for the current turn to finish before starting a review");
      await this.startReview(instructions);
    });
    this.submission = request.catch(() => {});
    return request;
  }

  private async startReview(instructions: string): Promise<void> {
    if (!this.rpc || this.cancelInProgress) throw new Error("No active Codex connection");
    const review = { started: false, children: new Set<string>(), pending: [] as Array<() => void> };
    this.reviewTurn = review;
    this.doneEmitted = false;
    this.metrics.turnCount++;
    const request = this.rpc.send("review/start", {
      threadId: this.threadId, target: { type: "custom", instructions },
    }, { timeoutMs: 0 }).then(result => {
      const turn = (result as { turn?: { id?: string } })?.turn;
      if (!turn?.id) throw new Error("review/start returned no turn.id");
      this.bindReviewTurn(review, turn.id);
    }).catch((error: unknown) => {
      if (error instanceof CodexRpcError) {
        if (this.reviewTurn === review) this.reviewTurn = null;
        if (this.activeTurnId === null) this.doneEmitted = true;
        throw error;
      }
      throw new AgentSubmissionUncertainError(error);
    }).finally(() => { this.startingTurn = null; });
    this.startingTurn = request;
    return request;
  }

  private bindReviewTurn(review: NonNullable<CodexAppServerAdapter["reviewTurn"]>, id: string): void {
    if (this.reviewTurn !== review || review.id) return;
    review.id = id;
    this.activeTurnId = id;
    for (const receive of review.pending.splice(0)) receive();
  }

  // ─── Command Execution ─────────────────────────────────────────────────

  /** Execute a command in the app-server sandbox context. Returns a sessionId. */
  async execCommand(params: {
    command: string;
    args?: string[];
    cwd?: string;
    env?: Record<string, string>;
    stdin?: string;
    timeoutMs?: number;
  }): Promise<{ sessionId: string }> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const sessionId = randomUUID();
    const requestParams: Record<string, unknown> = {
      processId: sessionId,
      command: [params.command, ...(params.args ?? [])],
      tty: true,
    };
    if (params.cwd) requestParams.cwd = params.cwd;
    if (params.env) requestParams.env = params.env;
    if (params.timeoutMs !== undefined) requestParams.timeoutMs = params.timeoutMs;

    const session = { stdout: new StringDecoder("utf8"), stderr: new StringDecoder("utf8") };
    this.commandSessions.set(sessionId, session);
    // The response arrives at process exit, not at startup. Return the client
    // process id now so stdin/resize/terminate can run while execution is pending.
    this.rpc.send("command/exec", requestParams, { timeoutMs: 0 }).then(result => {
      const completion = result as { exitCode: number };
      const tail = session.stdout.end() + session.stderr.end();
      if (tail) this.emitEvent({ type: "command_output", tool: "CommandSession", timestamp: Date.now(), commandSessionId: sessionId, content: tail });
      this.emitEvent({ type: "command_exited", tool: "CommandSession", timestamp: Date.now(), commandSessionId: sessionId, exitCode: completion.exitCode });
    }).catch((error: unknown) => {
      if (!this.cancelInProgress) {
        this.emitEvent({ type: "error", timestamp: Date.now(), commandSessionId: sessionId, error: error instanceof Error ? error.message : "Command execution failed" });
        this.emitEvent({ type: "command_exited", tool: "CommandSession", timestamp: Date.now(), commandSessionId: sessionId, exitCode: 1 });
      }
    }).finally(() => this.commandSessions.delete(sessionId));
    if (params.stdin !== undefined) await this.writeCommandStdin(sessionId, params.stdin, true);
    return { sessionId };
  }

  /** Write to stdin of a running command session. */
  async writeCommandStdin(
    sessionId: string,
    stdin: string,
    closeStdin?: boolean,
  ): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const params: Record<string, unknown> = { processId: sessionId, deltaBase64: Buffer.from(stdin, "utf8").toString("base64") };
    if (closeStdin !== undefined) params.closeStdin = closeStdin;
    await this.rpc.send("command/exec/write", params);
    return {};
  }

  /** Resize the PTY of a running command session. */
  async resizeCommandPty(
    sessionId: string,
    cols: number,
    rows: number,
  ): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("command/exec/resize", { processId: sessionId, size: { cols, rows } });
    return {};
  }

  /** Terminate a running command session. */
  async terminateCommand(sessionId: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("command/exec/terminate", { processId: sessionId });
    return {};
  }

  // ─── Feedback ──────────────────────────────────────────────────────────

  /** Upload turn feedback (rating, comment, tags). */
  async uploadFeedback(params: {
    turnId?: string;
    rating?: number;
    comment?: string;
    tags?: string[];
  }): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const requestParams: Record<string, unknown> = {
      threadId: this.threadId,
      classification: "other",
      reason: [params.comment, params.rating === undefined ? undefined : `Rating: ${params.rating}`].filter(Boolean).join("\n"),
    };
    if (params.tags) requestParams.tags = Object.fromEntries(params.tags.map((tag, index) => [String(index), tag]));
    await this.rpc.send("feedback/upload", requestParams);
    return {};
  }

  // ─── Filesystem ────────────────────────────────────────────────────────

  /** Read a file from the app-server's sandbox scope. */
  async fsReadFile(path: string): Promise<string> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("fs/readFile", { path }) as
      Record<string, unknown> | undefined;
    if (typeof result?.dataBase64 !== "string") throw new Error("fs/readFile returned no dataBase64");
    return Buffer.from(result.dataBase64, "base64").toString("utf8");
  }

  /** Write content to a file in the app-server's sandbox scope. */
  async fsWriteFile(path: string, content: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("fs/writeFile", { path, dataBase64: Buffer.from(content, "utf8").toString("base64") });
    return {};
  }

  /** Create a directory in the app-server's sandbox scope. */
  async fsCreateDirectory(path: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("fs/createDirectory", { path });
    return {};
  }

  /** Get file metadata from the app-server's sandbox scope. */
  async fsGetMetadata(path: string): Promise<Record<string, unknown>> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("fs/getMetadata", { path });
    return (result as Record<string, unknown>) ?? {};
  }

  /** List directory contents in the app-server's sandbox scope. */
  async fsReadDirectory(path: string): Promise<Array<Record<string, unknown>>> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("fs/readDirectory", { path }) as
      Record<string, unknown> | undefined;
    return (result?.entries as Array<Record<string, unknown>>) ?? [];
  }

  /** Remove a file or directory in the app-server's sandbox scope. */
  async fsRemove(path: string, recursive?: boolean): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const params: Record<string, unknown> = { path };
    if (recursive !== undefined) params.recursive = recursive;
    await this.rpc.send("fs/remove", params);
    return {};
  }

  /** Copy a file or directory in the app-server's sandbox scope. */
  async fsCopy(source: string, destination: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("fs/copy", { sourcePath: source, destinationPath: destination });
    return {};
  }

  /**
   * Watch a file or directory for changes.
   * Returns a watchId that can be used with fsUnwatch.
   * The adapter emits "filesystem_change" AgentEvents for fs/changed notifications.
   */
  async fsWatch(path: string): Promise<string> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const watchId = randomUUID();
    await this.rpc.send("fs/watch", { path, watchId });
    return watchId;
  }

  /** Stop watching a previously registered file watch. */
  async fsUnwatch(watchId: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("fs/unwatch", { watchId });
    return {};
  }

  private clearRequests(): void {
    for (const requestId of [...this.pendingApprovals.keys(), ...this.pendingInputs.keys()]) {
      this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
    }
    this.pendingApprovals.clear();
    this.pendingInputs.clear();
  }
}
