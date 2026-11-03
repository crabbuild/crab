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

import {
  AgentAdapter,
  getExpandedEnv,
  type AgentEvent,
} from "./orchestrator-adapters";
import { CodexRpcClient } from "./codex-rpc";
import {
  CODEX_TRACE_LINE_RE,
  CODEX_NOISE_LINE_RE,
  CODEX_MCP_NOISE_RE,
  stripAnsi,
  mapDecisionToCodex,
  parseCodexEvents,
  extractUsageEvent,
  type CodexParseState,
} from "./codex-shared";

// ─── Constants ───────────────────────────────────────────────────────────────

/** Timeout for the initialize handshake (ms). */
const INIT_TIMEOUT_MS = 10_000;

/** Grace period to wait for turn/completed after sending turn/interrupt. */
const CANCEL_GRACE_MS = 5_000;

/**
 * Timeout for a turn to produce its first event (turn/started or any
 * streaming delta). If the Codex server doesn't respond within this
 * window, we emit an error so the user isn't left staring at a blank
 * screen for minutes. The timeout is generous (2 minutes) to account
 * for cold-start model loading and API queue times.
 */
const TURN_FIRST_EVENT_TIMEOUT_MS = 120_000;

/** Time to wait after rpc.close() before sending SIGTERM. */
const CANCEL_SIGTERM_MS = 2_000;

/** Time to wait after SIGTERM before sending SIGKILL. */
const CANCEL_SIGKILL_MS = 5_000;

// ─── Internal Types ──────────────────────────────────────────────────────────

interface PendingApproval {
  /** The wire-level JSON-RPC request id we must respond to. */
  rpcId: number;
  /** The public, durable id surfaced on the AgentEvent. */
  requestId: string;
  /** The server-request method name. */
  method: string;
  /** Recorded for the approval-round-trip metric. */
  receivedAt: number;
}

interface ConnectionMetrics {
  initMs: number;
  turnCount: number;
  interruptCount: number;
  steerCount: number;
  steerFallbackCount: number;
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
  readonly binary = "codex";

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
  private initialized = false;
  private lastAgentMessage = "";
  private metrics: ConnectionMetrics = {
    initMs: 0,
    turnCount: 0,
    interruptCount: 0,
    steerCount: 0,
    steerFallbackCount: 0,
    approvalRoundTripMs: [],
    reconnectCount: 0,
    backpressureRejects: 0,
  };

  /** Guards against multiple concurrent cancel() calls. */
  private cancelInProgress = false;

  /** Resolves when turn/completed arrives (used by cancel and iterate fallback). */
  private turnCompletedResolve: (() => void) | null = null;

  /** Timer for detecting unresponsive turns. Cleared on first event. */
  private turnTimeoutTimer: ReturnType<typeof setTimeout> | null = null;

  /** Tracks whether a usage event has been emitted for the current turn (avoids double-counting). */
  private usageEmittedForTurn = false;

  /** Tracks active command/exec sessions (sessionId → metadata). */
  private commandSessions: Map<string, { receivedAt: number }> = new Map();

  constructor(extraArgs?: string[], optOutNotifications?: string[]) {
    super();
    this.extraArgs = extraArgs ?? [];
    this.optOutNotifications = optOutNotifications ?? [];
  }

  // ─── AgentAdapter contract ─────────────────────────────────────────────────

  /**
   * Spawn the codex app-server process and start the first turn, or
   * start a new turn on an already-live connection (adapter reuse
   * across iterations).
   */
  spawn(prompt: string, cwd: string, env?: Record<string, string>): void {
    // If already initialized (reuse across iterations), just start a new turn.
    if (this.initialized && this.rpc) {
      this.startTurn(prompt);
      return;
    }

    this.doSpawn(prompt, cwd, env);
  }

  sendMessage(_text: string): void {
    // No-op: the orchestrator uses iterate() for follow-ups.
  }

  /**
   * Send a follow-up message on the live connection without re-spawning.
   * If a turn is active, attempts `turn/steer`; on failure falls back to
   * `turn/interrupt` then `turn/start`. If no turn is active, starts a
   * fresh turn directly.
   */
  iterate(prompt: string): void {
    if (!this.rpc) return;

    if (this.activeTurnId !== null) {
      // Turn is active — try turn/steer first.
      this.metrics.steerCount++;
      this.rpc
        .send("turn/steer", {
          threadId: this.threadId,
          turnId: this.activeTurnId,
          input: [{ type: "text", text: prompt }],
        })
        .then(() => {
          this.metrics.turnCount++;
        })
        .catch(() => {
          // Steer failed — fall back to interrupt + start.
          this.metrics.steerFallbackCount++;
          this.steerFallback(prompt);
        });
    } else {
      // No active turn — start a fresh one.
      this.startTurn(prompt);
    }
  }

  /**
   * Deliver the user's approval decision live over the JSON-RPC connection.
   * Maps Crab's decision vocabulary to the Codex protocol's and responds
   * to the original server-initiated request. No-op if the request has
   * already been resolved (e.g. renderer raced with shutdown).
   */
  respondApproval(requestId: string, decision: "allow" | "allow_once" | "deny"): void {
    const pending = this.pendingApprovals.get(requestId);
    if (!pending) {
      // Renderer raced with shutdown — no-op is safe.
      return;
    }
    if (!this.rpc) return;

    const codexDecision = mapDecisionToCodex(decision);
    this.rpc.respond(pending.rpcId, { decision: codexDecision });
    this.metrics.approvalRoundTripMs.push(Date.now() - pending.receivedAt);
    this.pendingApprovals.delete(requestId);
  }

  /**
   * Gracefully shut down the adapter. Cancels pending approvals, interrupts
   * any active turn, unsubscribes from the thread, closes the RPC transport,
   * then escalates through SIGTERM → SIGKILL if the process doesn't exit.
   * Idempotent — a second call while shutdown is in progress is a no-op.
   */
  cancel(): void {
    this.clearTurnTimeout();
    if (this.cancelInProgress) return; // Idempotent
    this.cancelInProgress = true;

    this.doCancel().catch(() => {
      // Best-effort shutdown; errors are swallowed.
    });
  }

  // ─── Private: spawn and initialization ─────────────────────────────────────

  private doSpawn(prompt: string, cwd: string, env?: Record<string, string>): void {
    const args = ["app-server", "--listen", "stdio://", ...this.extraArgs];

    this.proc = spawn(this.binary, args, {
      cwd,
      env: getExpandedEnv(env),
      stdio: ["pipe", "pipe", "pipe"],
    });

    // Construct RPC client over stdin/stdout.
    this.rpc = new CodexRpcClient(this.proc.stdin!, this.proc.stdout!, {
      requestTimeoutMs: INIT_TIMEOUT_MS,
      logger: (msg) => {
        if (process.env.CRAB_CODEX_DEBUG) {
          console.error(`[codex-rpc] ${msg}`);
        }
      },
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

    // Wire process exit.
    this.proc.on("exit", (code) => {
      const exitCode = code ?? 1;
      const hadActiveTurn = this.activeTurnId !== null;
      this.rpc = null;
      this.initialized = false;
      this.activeTurnId = null;

      if (hadActiveTurn) {
        // Mid-turn process death: emit error + done("") before exit (CA6.3).
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: "Codex app-server process exited mid-turn",
        });
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content: "",
        });
      }

      this.emitExit(exitCode);
    });

    this.proc.on("error", (err: NodeJS.ErrnoException) => {
      let message = err.message;
      if (err.code === "ENOENT") {
        message = `Agent binary "${this.binary}" not found on PATH. Please install it or configure the path in Settings → Agents.`;
      }
      this.emitEvent({
        type: "error",
        timestamp: Date.now(),
        error: message,
      });
      this.emitExit(1);
      this.proc = null;
    });

    // Run the initialization handshake.
    this.initializeAndStart(prompt);
  }

  private async initializeAndStart(prompt: string): Promise<void> {
    if (!this.rpc) return;

    const initStart = Date.now();

    try {
      // Step 1: Initialize handshake.
      const capabilities: Record<string, unknown> = {};
      if (this.optOutNotifications.length > 0) {
        capabilities.optOutNotificationMethods = this.optOutNotifications;
      }

      await this.rpc.send("initialize", {
        clientInfo: {
          name: "crab_desktop",
          title: "Crab Desktop",
          version: "0.1.0",
        },
        capabilities,
      });

      // Step 2: Send initialized notification.
      this.rpc.notify("initialized");
      this.metrics.initMs = Date.now() - initStart;
    } catch (err: unknown) {
      // Initialize failed or timed out.
      const message =
        err instanceof Error ? err.message : "Unknown initialization error";
      this.emitEvent({
        type: "error",
        timestamp: Date.now(),
        error: `Codex app-server initialization failed: ${message}. ` +
          "Please ensure your Codex binary supports app-server mode (codex ≥ 0.1.0).",
      });
      this.emitExit(1);
      return;
    }

    // Step 3: Start or resume thread.
    try {
      await this.startOrResumeThread();
    } catch (err: unknown) {
      const message =
        err instanceof Error ? err.message : "Thread start failed";
      this.emitEvent({
        type: "error",
        timestamp: Date.now(),
        error: `Codex thread start failed: ${message}`,
      });
      this.cancel();
      this.emitExit(1);
      return;
    }
    this.initialized = true;

    // Step 4: Wire notification handlers (placeholder for task 3.6).
    this.wireNotificationHandlers();

    // Step 5: Wire server-request handler (placeholder for task 3.3).
    this.wireServerRequestHandler();

    // Step 6: Start the first turn.
    this.startTurn(prompt);
  }

  private async startOrResumeThread(): Promise<void> {
    if (!this.rpc) return;

    const method = this.resumeThreadId ? "thread/resume" : "thread/start";
    const params = this.resumeThreadId ? { threadId: this.resumeThreadId } : {};
    // Resume must load the stored thread before turn/start. A failure must
    // surface to the user; starting a fresh thread would silently lose context.
    const result = await this.rpc.send(method, params) as { thread?: { id?: unknown } } | null;
    const threadId = result?.thread?.id;
    if (typeof threadId !== "string" || !threadId) {
      throw new Error(`${method} returned no thread.id`);
    }
    this.threadId = threadId;
    this.resumeThreadId = null;
  }

  // ─── Private: turn management ──────────────────────────────────────────────

  private startTurn(prompt: string): void {
    if (!this.rpc) return;

    this.metrics.turnCount++;

    // Arm a timeout: if the server doesn't produce any event (turn/started,
    // streaming delta, or turn/completed) within the window, surface an
    // error so the user knows something is wrong rather than waiting
    // indefinitely. The timer is cleared by the first notification handler
    // that fires for this turn.
    this.clearTurnTimeout();
    this.turnTimeoutTimer = setTimeout(() => {
      this.turnTimeoutTimer = null;
      // Only fire if we're still waiting (no turn activity observed).
      if (this.activeTurnId === null && this.rpc) {
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error:
            "Codex is not responding. The model may be overloaded, or the API key " +
            "may be invalid. Check your network connection and OPENAI_API_KEY.",
        });
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content: "",
        });
      }
    }, TURN_FIRST_EVENT_TIMEOUT_MS);

    this.rpc
      .send("turn/start", {
        threadId: this.threadId,
        input: [{ type: "text", text: prompt }],
      })
      .catch((err: unknown) => {
        this.clearTurnTimeout();
        const message =
          err instanceof Error ? err.message : "turn/start failed";
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: `Codex turn/start failed: ${message}`,
        });
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content: "",
        });
      });
  }

  private async steerFallback(prompt: string): Promise<void> {
    if (!this.rpc || !this.activeTurnId) return;

    try {
      // Interrupt the current turn.
      this.metrics.interruptCount++;
      this.rpc.send("turn/interrupt", {
        threadId: this.threadId,
        turnId: this.activeTurnId,
      }).catch(() => {
        // Best-effort — the interrupt may fail if the connection is
        // already closed, but we proceed to start a new turn regardless.
      });

      // Wait for turn/completed (with a timeout).
      await this.waitForTurnCompleted(CANCEL_GRACE_MS);
    } catch {
      // Best-effort: proceed to start a new turn regardless.
    }

    // Start a fresh turn with the new prompt.
    this.startTurn(prompt);
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

  /**
   * Wires notification handlers for the Codex app-server protocol.
   * Full implementation in task 3.6; this provides the minimal set
   * needed for core lifecycle.
   */
  private wireNotificationHandlers(): void {
    if (!this.rpc) return;

    const state: CodexParseState = { lastAgentMessage: "" };

    // Thread lifecycle
    this.rpc.onNotification("thread/started", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const thread = p?.thread as Record<string, unknown> | undefined;
      const threadId = String(thread?.id ?? p?.threadId ?? "");
      if (threadId) {
        this.threadId = threadId;
        this.emitEvent({
          type: "thread_started",
          timestamp: Date.now(),
          threadId,
        });
      }
    });

    // Turn lifecycle
    this.rpc.onNotification("turn/started", (params: unknown) => {
      this.clearTurnTimeout();
      const p = params as Record<string, unknown> | undefined;
      const turn = p?.turn as Record<string, unknown> | undefined;
      const turnId = String(turn?.id ?? p?.turnId ?? "");
      if (turnId) {
        this.activeTurnId = turnId;
      }
      this.usageEmittedForTurn = false;
      this.doneEmitted = false;
      this.lastAgentMessage = "";
      state.lastAgentMessage = "";
      state.turnOpen = true;
    });

    this.rpc.onNotification("turn/completed", (params: unknown) => {
      this.clearTurnTimeout();
      this.activeTurnId = null;

      // Resolve any pending waitForTurnCompleted.
      if (this.turnCompletedResolve) {
        this.turnCompletedResolve();
      }

      // Extract usage and emit done.
      const p = params as Record<string, unknown> | undefined;
      const turn = p?.turn as Record<string, unknown> | undefined;
      const status = String(turn?.status ?? p?.status ?? "completed");
      const usage = (p?.usage ?? turn?.usage) as Record<string, unknown> | undefined;

      if (status === "completed") {
        if (usage) {
          const usageEvent = extractUsageEvent(usage);
          if (usageEvent) {
            this.emitEvent(usageEvent);
            this.usageEmittedForTurn = true;
          }
        }
        const content = this.lastAgentMessage;
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content,
        });
        state.lastCompletedAgentMessage = content;
        state.lastAgentMessage = "";
        state.turnOpen = false;
        this.lastAgentMessage = "";
      } else if (status === "interrupted") {
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: "Turn interrupted.",
        });
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content: "",
        });
        state.lastCompletedAgentMessage = "";
        state.lastAgentMessage = "";
        state.turnOpen = false;
      } else if (status === "failed") {
        const turnError = turn?.error as Record<string, unknown> | undefined;
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: String(turnError?.message ?? "Codex turn failed"),
        });
        // Emit done so the UI closes the turn and doesn't hang.
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content: "",
        });
        state.lastCompletedAgentMessage = "";
        state.lastAgentMessage = "";
        state.turnOpen = false;
      }
    });

    // Streaming deltas — route through parseCodexEvents for full mapping.
    // These handlers cover the most common notification methods.
    this.rpc.onNotification("item/agentMessage/delta", (params: unknown) => {
      this.clearTurnTimeout();
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.lastAgentMessage += delta;
        // Keep the shared parse state in sync so that item/completed
        // dedup (in parseCodexItem) recognises the accumulated text and
        // doesn't re-emit the full message as a duplicate text event.
        state.lastAgentMessage = this.lastAgentMessage;
        this.emitEvent({ type: "text", timestamp: Date.now(), content: delta });
      }
    });

    this.rpc.onNotification("item/reasoning/summaryTextDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.emitEvent({ type: "thinking", timestamp: Date.now(), content: delta });
      }
    });

    this.rpc.onNotification("item/reasoning/textDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.emitEvent({ type: "thinking", timestamp: Date.now(), content: delta });
      }
    });

    this.rpc.onNotification("item/plan/delta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const delta = String(p?.delta ?? "");
      if (delta) {
        this.emitEvent({ type: "thinking", timestamp: Date.now(), content: delta });
      }
    });

    // Terminal interaction updates during command execution — informational,
    // no event emission needed. Silently acknowledge to suppress the
    // "Unknown notification method" warning.
    this.rpc.onNotification("item/commandExecution/terminalInteraction", () => {
      // Intentionally empty — terminal interaction data is not surfaced
      // in the chat UI. The command output arrives via outputDelta.
    });

    this.rpc.onNotification("item/commandExecution/outputDelta", (params: unknown) => {
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

    this.rpc.onNotification("item/fileChange/outputDelta", (params: unknown) => {
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

    this.rpc.onNotification("item/completed", (params: unknown) => {
      // Route through parseCodexEvents for full item parsing.
      const p = params as Record<string, unknown> | undefined;
      if (!p) return;
      const raw: Record<string, unknown> = { method: "item/completed", params: p };
      const events = parseCodexEvents(raw, state);
      for (const event of events) {
        this.emitEvent(event);
      }
      // Sync lastAgentMessage from the shared state.
      if (this.activeTurnId && state.lastAgentMessage) {
        this.lastAgentMessage = state.lastAgentMessage;
      }
    });

    this.rpc.onNotification("item/started", (params: unknown) => {
      // Route through parseCodexEvents to emit tool_use events so the
      // renderer can show tool cards (Bash, Write, etc.) when items begin.
      const p = params as Record<string, unknown> | undefined;
      if (!p) return;
      const raw: Record<string, unknown> = { method: "item/started", params: p };
      const events = parseCodexEvents(raw, state);
      for (const event of events) {
        this.emitEvent(event);
      }
    });

    // Mid-turn error notifications (D11).
    this.rpc.onNotification("error", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const willRetry = (p as any)?.willRetry === true;
      const errorObj = (p as any)?.error as Record<string, unknown> | undefined;
      const message = String(errorObj?.message ?? p?.message ?? "Codex reported an error");
      const code = errorObj?.code ?? (p as any)?.code;

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
    this.rpc.onNotification("turn/plan/updated", (params: unknown) => {
      this.emitEvent({
        type: "task",
        timestamp: Date.now(),
        tool: "EnterPlanMode",
        input: params,
      });
    });

    // model/rerouted
    this.rpc.onNotification("model/rerouted", (params: unknown) => {
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
      const requestId = String(p?.requestId ?? p?.request_id ?? "");
      if (requestId && this.pendingApprovals.has(requestId)) {
        this.pendingApprovals.delete(requestId);
      }
    });

    // thread/closed — trigger graceful shutdown.
    this.rpc.onNotification("thread/closed", () => {
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
    this.rpc.onNotification("thread/tokenUsage/updated", (params: unknown) => {
      // Emit usage only if no usage has been emitted for this turn yet
      // (avoids double-counting when both this and turn/completed carry usage).
      if (this.usageEmittedForTurn) return;
      const p = params as Record<string, unknown> | undefined;
      // Wire shape (camelCase): { threadId, turnId, tokenUsage: { last, total, modelContextWindow } }.
      // We prefer `last` (this turn's delta) and fall back to `total`.
      // See ThreadTokenUsageUpdatedNotification / TokenUsageBreakdown:
      //   https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/thread.rs
      const tokenUsage = (p?.tokenUsage ?? p?.token_usage ?? p?.usage) as
        Record<string, unknown> | undefined;
      const breakdown = (tokenUsage?.last ?? tokenUsage?.total ?? tokenUsage) as
        Record<string, unknown> | undefined;
      if (breakdown) {
        const usageEvent = extractUsageEvent(breakdown);
        if (usageEvent) {
          this.emitEvent(usageEvent);
          this.usageEmittedForTurn = true;
        }
      }
    });

    // command/exec/outputDelta — stream command output by session
    this.rpc.onNotification("command/exec/outputDelta", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const sessionId = String(p?.sessionId ?? p?.session_id ?? "");
      const delta = String(p?.delta ?? p?.output ?? p?.text ?? "");
      this.emitEvent({
        type: "command_output",
        tool: "CommandSession",
        timestamp: Date.now(),
        commandSessionId: sessionId,
        content: delta,
      });
    });

    // command/exec/completed — clean up session tracking
    this.rpc.onNotification("command/exec/completed", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const sessionId = String(p?.sessionId ?? p?.session_id ?? "");
      const exitCode = typeof p?.exitCode === "number" ? p.exitCode : null;
      this.commandSessions.delete(sessionId);
      this.emitEvent({
        type: "command_exited",
        tool: "CommandSession",
        timestamp: Date.now(),
        commandSessionId: sessionId,
        exitCode: exitCode ?? undefined,
      });
    });

    // fs/changed — relay filesystem watch events
    this.rpc.onNotification("fs/changed", (params: unknown) => {
      const p = params as Record<string, unknown> | undefined;
      const watchId = String(p?.watchId ?? p?.watch_id ?? "");
      const eventPath = String(p?.path ?? "");
      const kind = String(p?.kind ?? p?.changeType ?? "");
      this.emitEvent({
        type: "filesystem_change",
        tool: "FileWatchNotification",
        timestamp: Date.now(),
        path: eventPath,
        fsWatchId: watchId,
        fsChangeKind: kind,
      });
    });

    // Unknown notification methods: log debug and drop (CA7.3).
    this.rpc.onUnhandledNotification((method: string) => {
      console.debug(`[CodexAppServerAdapter] Unknown notification method: ${method}`);
    });
  }

  /**
   * Wires the server-request handler for approval prompts.
   * Full implementation in task 3.3; this provides a minimal stub
   * that handles approval requests and rejects unknown methods.
   */
  private wireServerRequestHandler(): void {
    if (!this.rpc) return;

    this.rpc.onServerRequest((id: number, method: string, params: unknown) => {
      // Handle approval requests.
      if (
        method === "item/commandExecution/requestApproval" ||
        method === "item/fileChange/requestApproval"
      ) {
        const p = params as Record<string, unknown> | undefined;
        const kind: NonNullable<AgentEvent["approval"]>["kind"] =
          method === "item/fileChange/requestApproval" ? "file" : "command";

        const requestId = String(
          p?.request_id ?? p?.requestId ?? `${method}-${Date.now()}`,
        );
        const cmd = String(p?.command ?? "");
        const filePath = String(p?.path ?? p?.file ?? "");
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

        // Store the pending approval for later response.
        this.pendingApprovals.set(requestId, {
          rpcId: id,
          requestId,
          method,
          receivedAt: Date.now(),
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
        this.rpc.respond(pending.rpcId, { decision: "cancel" });
      }
    }
    this.pendingApprovals.clear();

    // Step 2: Interrupt active turn if any.
    if (this.activeTurnId !== null && this.rpc) {
      try {
        this.rpc.send("turn/interrupt", {
          threadId: this.threadId,
          turnId: this.activeTurnId,
        });
        await this.waitForTurnCompleted(CANCEL_GRACE_MS);
      } catch {
        // Best-effort.
      }
    }

    // Step 3: Unsubscribe from thread (best-effort).
    if (this.rpc && this.threadId) {
      try {
        this.rpc.notify("thread/unsubscribe", { threadId: this.threadId });
      } catch {
        // Best-effort.
      }
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

  /** Clear the turn-first-event timeout (called when any turn activity arrives). */
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

  /** Rollback the last n turns of the current thread. */
  async rollbackThread(n: number): Promise<{ threadId: string }> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("thread/rollback", {
      threadId: this.threadId,
      n,
    }) as Record<string, unknown> | undefined;
    const thread = (result?.thread ?? result) as Record<string, unknown> | undefined;
    const newId = String(thread?.id ?? this.threadId ?? "");
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
  async setGoal(goal: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/goal/set", {
      threadId: this.threadId,
      goal,
    });
    return {};
  }

  /** Get the current thread's goal. */
  async getGoal(): Promise<{ goal: string; tokenBudget?: number }> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("thread/goal/get", {
      threadId: this.threadId,
    }) as Record<string, unknown> | undefined;
    return {
      goal: String(result?.goal ?? ""),
      tokenBudget: typeof result?.tokenBudget === "number" ? result.tokenBudget : undefined,
    };
  }

  /** Clear the current thread's goal. */
  async clearGoal(): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/goal/clear", {
      threadId: this.threadId,
    });
    return {};
  }

  /** Update arbitrary metadata on the current thread. */
  async updateThreadMetadata(metadata: Record<string, unknown>): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("thread/metadata/update", {
      threadId: this.threadId,
      metadata,
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

  /** Start review mode on the current thread. */
  async startReview(): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("review/start", {
      threadId: this.threadId,
    });
    return {};
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
    const requestParams: Record<string, unknown> = {
      threadId: this.threadId,
      command: params.command,
    };
    if (params.args) requestParams.args = params.args;
    if (params.cwd) requestParams.cwd = params.cwd;
    if (params.env) requestParams.env = params.env;
    if (params.stdin !== undefined) requestParams.stdin = params.stdin;
    if (params.timeoutMs !== undefined) requestParams.timeoutMs = params.timeoutMs;

    const result = await this.rpc.send("command/exec", requestParams) as
      Record<string, unknown> | undefined;
    const sessionId = String(result?.sessionId ?? result?.session_id ?? "");
    if (!sessionId) throw new Error("command/exec did not return a sessionId");

    this.commandSessions.set(sessionId, { receivedAt: Date.now() });
    return { sessionId };
  }

  /** Write to stdin of a running command session. */
  async writeCommandStdin(
    sessionId: string,
    stdin: string,
    closeStdin?: boolean,
  ): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const params: Record<string, unknown> = { sessionId, stdin };
    if (closeStdin !== undefined) params.closeStdin = closeStdin;
    this.rpc.notify("command/exec/write", params);
    return {};
  }

  /** Resize the PTY of a running command session. */
  async resizeCommandPty(
    sessionId: string,
    cols: number,
    rows: number,
  ): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    this.rpc.notify("command/exec/resize", { sessionId, cols, rows });
    return {};
  }

  /** Terminate a running command session. */
  async terminateCommand(sessionId: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    this.rpc.notify("command/exec/terminate", { sessionId });
    this.commandSessions.delete(sessionId);
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
      turnId: params.turnId ?? this.activeTurnId,
    };
    if (params.rating !== undefined) requestParams.rating = params.rating;
    if (params.comment !== undefined) requestParams.comment = params.comment;
    if (params.tags) requestParams.tags = params.tags;
    await this.rpc.send("feedback/upload", requestParams);
    return {};
  }

  // ─── Filesystem ────────────────────────────────────────────────────────

  /** Read a file from the app-server's sandbox scope. */
  async fsReadFile(path: string): Promise<string> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("fs/read", { path }) as
      Record<string, unknown> | undefined;
    return String(result?.content ?? result?.text ?? result ?? "");
  }

  /** Write content to a file in the app-server's sandbox scope. */
  async fsWriteFile(path: string, content: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("fs/write", { path, content });
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
    const result = await this.rpc.send("fs/metadata", { path });
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
    await this.rpc.send("fs/copy", { source, destination });
    return {};
  }

  /**
   * Watch a file or directory for changes.
   * Returns a watchId that can be used with fsUnwatch.
   * The adapter emits "filesystem_change" AgentEvents for fs/changed notifications.
   */
  async fsWatch(path: string): Promise<string> {
    if (!this.rpc) throw new Error("No active Codex connection");
    const result = await this.rpc.send("fs/watch", { path }) as
      Record<string, unknown> | undefined;
    const watchId = String(result?.watchId ?? result?.id ?? "");
    if (!watchId) throw new Error("fs/watch did not return a watchId");
    return watchId;
  }

  /** Stop watching a previously registered file watch. */
  async fsUnwatch(watchId: string): Promise<{}> {
    if (!this.rpc) throw new Error("No active Codex connection");
    await this.rpc.send("fs/unwatch", { watchId });
    return {};
  }

  private clearTurnTimeout(): void {
    if (this.turnTimeoutTimer !== null) {
      clearTimeout(this.turnTimeoutTimer);
      this.turnTimeoutTimer = null;
    }
  }
}
