/**
 * Shared Codex parsing utilities.
 *
 * Contains the regex filters, event parsers, and decision mapping used
 * by the CodexAppServerAdapter (app-server mode). Extracted here so the
 * ~600-line item dispatcher lives in one place and both adapters share
 * identical parsing logic.
 */

import type { AgentEvent } from "./orchestrator-adapters";

// ─── Stderr Filters ──────────────────────────────────────────────────────────

/**
 * Strips ANSI/CSI escape sequences from a line.
 *
 * Codex's `tracing` output is colourised when it detects a tty, and even
 * when it doesn't, some terminal-aware shells inject SGR sequences. The
 * raw bytes look like `\x1b[2m...\x1b[0m`, but tools that pre-strip the
 * ESC byte leave behind the literal `[2m` / `[0m` markers — so we scrub
 * both forms before applying line-shape matchers.
 *
 * The regex matches:
 *   1. Real CSI sequences: ESC `[` … final byte (`m`, `K`, `J`, etc.).
 *   2. Bare CSI tails: a leading `[` followed by digits / `;` and a
 *      terminator letter, with no preceding ESC. This catches the
 *      `[2mERROR[0m` shape we see in the wild.
 *
 * We intentionally do NOT strip the bare form unless it parses as a
 * complete SGR-ish sequence (`[<digits/semicolons>{m,K,J,H,A,B,C,D,f}`)
 * so we don't eat legitimate text that happens to start with `[`.
 */
export function stripAnsi(line: string): string {
  return line
    // Real ESC-prefixed CSI: \x1b[ ... <terminator>
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")
    // Bare CSI shape (ESC was eaten by upstream tooling):
    //   `[2m`, `[0m`, `[31;1m`, etc. Restricted to SGR-ish terminators
    //   so plain bracketed text isn't damaged.
    .replace(/\[[0-9;]*[mKJHfABCDsuG]/g, "");
}

/**
 * Matches Codex's internal tracing lines emitted on stderr, e.g.
 * `2026-05-08T19:16:51.208414Z ERROR codex_api::endpoint::responses_websocket: ...`.
 * These are diagnostic noise from reconnect/retry loops; the actual user-
 * visible error is already delivered as `{"type":"error","message":"..."}`
 * on stdout, so we filter them out to avoid duplicate reporting.
 *
 * The line MAY be prefixed with ANSI / CSI escape sequences (real `\x1b[…m`
 * or stripped-ESC `[…m`); use `stripAnsi()` before testing if you need to
 * tolerate those.
 */
export const CODEX_TRACE_LINE_RE =
  /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+/;

/**
 * Matches stderr lines from the rmcp transport / service workers that
 * Codex starts up for OAuth-protected MCP servers (Sentry, Linear, Neon,
 * Superset, …). When the user hasn't authenticated, the workers log a
 * `Transport channel closed, when AuthRequired(...)` error on every
 * startup. Those are not user-actionable from the chat surface — they
 * belong in the MCP settings UI — so we drop them outright.
 */
export const CODEX_MCP_NOISE_RE =
  /\b(rmcp::transport::worker|rmcp::service)\b/;

/**
 * Lines we explicitly suppress from Codex stderr. These are status / progress
 * lines the CLI prints when running attached to a TTY but that aren't useful
 * (and are misleading) inside the chat surface where the agent runs headless.
 *
 * - "Reading additional input from stdin..." is printed when Codex thinks more
 *   stdin data is coming. We pass `stdio: ["ignore", ...]` to prevent it, but
 *   we keep this filter as a defensive net for users on older binaries.
 * - "Reading prompt from stdin..." is the matching legacy phrasing.
 */
export const CODEX_NOISE_LINE_RE =
  /^(Reading additional input from stdin|Reading prompt from stdin)/i;

// ─── Decision Mapping ────────────────────────────────────────────────────────

/**
 * Maps Crab Desktop's approval vocabulary to the Codex protocol's
 * decision strings.
 *
 * | Crab decision | Codex decision     | Semantics                          |
 * |---------------|--------------------|------------------------------------|
 * | `allow`       | `acceptForSession` | Standing — Codex caches the rule.  |
 * | `allow_once`  | `accept`           | This invocation only.              |
 * | `deny`        | `decline`          | Reject; turn continues if possible.|
 * | `cancel`      | `cancel`           | Reject and interrupt (shutdown).   |
 */
export function mapDecisionToCodex(d: "allow" | "allow_once" | "deny" | "cancel"): string {
  switch (d) {
    case "allow":
      return "acceptForSession";
    case "allow_once":
      return "accept";
    case "deny":
      return "decline";
    case "cancel":
      return "cancel";
    default: {
      throw new Error("unreachable decision");
    }
  }
}


// ─── Shared State Interface ──────────────────────────────────────────────────

/**
 * Mutable state that the parsing functions read and write. Each adapter
 * instance owns one of these; the free functions receive it by reference.
 */
export interface CodexParseState {
  /** Text accumulated for the currently streaming assistant message. */
  lastAgentMessage: string;
  /**
   * Last assistant message that already closed with a turn/completed.
   * Codex can deliver the matching item/completed envelope after the
   * turn has closed; keep this around so that replay is ignored instead
   * of being treated as the first text of the next turn.
   */
  lastCompletedAgentMessage?: string;
  /** True after turn.started and false after turn.completed. */
  turnOpen?: boolean;
  /** Fires when the dup-text dedup branch suppresses a redundant
   *  `item.completed` text event. Wired by the adapter to increment
   *  the renderer-local `dupTextCollisions` diagnostic counter. */
  onDupTextCollision?: () => void;
}

// ─── Event Parsing ───────────────────────────────────────────────────────────

/**
 * Builds a `file_edit` AgentEvent from a single Codex `file_change`
 * payload. Routes `kind` and `status` so the renderer picks the
 * right card affordances.
 *
 * The wire shape for `kind` from app-server is an internally tagged
 * enum (`{"type": "add"}` / `{"type": "delete"}` /
 * `{"type": "update", "movePath": null}`). We also accept the legacy
 * flat-string form (`"add"` / `"delete"` / `"update"`) for backward
 * compatibility with older test fixtures. See:
 *   https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/item.rs
 *   (PatchChangeKind, FileUpdateChange)
 */
function buildFileChangeEvent(
  change: Record<string, unknown>,
  itemStatus: string,
  itemId: string,
  timestamp: number,
): AgentEvent {
  const path = String(change.path ?? change.file ?? "");

  // `kind` may be either a flat string or `{type: "add"|"delete"|"update", ...}`.
  const rawKind = change.kind;
  let kind: string;
  if (rawKind && typeof rawKind === "object") {
    kind = String((rawKind as Record<string, unknown>).type ?? "update").toLowerCase();
  } else {
    kind = String(rawKind ?? "update").toLowerCase();
  }

  const diff = String(change.diff ?? change.content ?? "");
  const status = String(change.status ?? itemStatus ?? "completed");

  let tool: string;
  if (kind === "add") tool = "Write";
  else if (kind === "delete") tool = "FileDelete";
  else tool = "Edit";

  return {
    type: "file_edit",
    timestamp,
    tool,
    path,
    diff,
    input: {
      file_path: path,
      kind,
      status,
      itemId,
      content: diff,
    },
  };
}

/**
 * Maps a Codex JSONL line to zero or more AgentEvents. A single line
 * can produce a content event plus a usage event (e.g. turn.completed
 * carries both a status and token usage).
 *
 * Supports two wire formats:
 *   1. CLI format (`codex exec --json`): `{"type":"item.started","item":{...}}`
 *   2. App-server format (WebSocket): `{"method":"item/started","params":{"item":{...}}}`
 *
 * Both are normalised to the same internal handling.
 */
export function parseCodexEvents(
  raw: Record<string, unknown>,
  state: CodexParseState,
): AgentEvent[] {
  const timestamp = Date.now();

  // Normalise: support both `type`-based (CLI) and `method`-based (app-server) formats.
  let type = raw.type as string | undefined;
  if (!type && raw.method) {
    // Convert method format: "item/started" → "item.started", "turn/completed" → "turn.completed"
    type = String(raw.method).replace(/\//g, ".");
    // For method-based format, the payload is in `params`.
    const params = raw.params as Record<string, unknown> | undefined;
    if (params) {
      // Merge params fields into raw for uniform access below.
      if (params.item) (raw as any).item = params.item;
      if (params.turn) (raw as any).turn = params.turn;
      if (params.usage) (raw as any).usage = params.usage;
      if (params.delta !== undefined) (raw as any).delta = params.delta;
      if (params.itemId) (raw as any).itemId = params.itemId;
      if (params.threadId) (raw as any).thread_id = params.threadId;
    }
  }

  switch (type) {
    case "thread.started": {
      const threadId = String(
        raw.thread_id ?? raw.threadId ?? "",
      );
      if (!threadId) return [];
      return [{
        type: "thread_started",
        timestamp,
        threadId,
      }];
    }
    case "turn.started":
      state.turnOpen = true;
      state.lastAgentMessage = "";
      return [];

    case "turn.completed": {
      const out: AgentEvent[] = [];
      const turn = raw.turn as Record<string, unknown> | undefined;
      const usage = (raw.usage ?? turn?.usage) as Record<string, unknown> | undefined;
      if (usage) {
        const usageEvent = extractUsageEvent(usage);
        if (usageEvent) out.push(usageEvent);
      }
      const content = state.lastAgentMessage;
      out.push({
        type: "done",
        timestamp,
        content,
      });
      state.lastCompletedAgentMessage = content;
      state.lastAgentMessage = "";
      state.turnOpen = false;
      return out;
    }

    case "turn.failed":
    case "error":
      state.lastAgentMessage = "";
      state.turnOpen = false;
      return [{
        type: "error",
        timestamp,
        error: String(
          (raw.error as Record<string, unknown> | undefined)?.message
            ?? raw.message ?? raw.error ?? "Codex reported an error",
        ),
      }];

    case "item.started":
    case "item.updated":
    case "item.completed":
      return parseCodexItem(raw, timestamp, type, state);

    // ── Streaming delta events ──────────────────────────────────────
    case "item.agentMessage.delta": {
      const delta = String(raw.delta ?? "");
      if (!delta) return [];
      state.lastAgentMessage += delta;
      return [{ type: "text", timestamp, content: delta }];
    }

    case "item.commandExecution.outputDelta": {
      const delta = String(raw.delta ?? "");
      if (!delta) return [];
      return [{
        type: "tool_result",
        timestamp,
        tool: "Bash",
        itemId: raw.itemId ? String(raw.itemId) : undefined,
        output: delta,
      }];
    }

    case "item.fileChange.outputDelta": {
      const delta = String(raw.delta ?? "");
      if (!delta) return [];
      return [{
        type: "tool_result",
        timestamp,
        tool: "Write",
        itemId: raw.itemId ? String(raw.itemId) : undefined,
        output: delta,
      }];
    }

    case "item.reasoning.summaryTextDelta":
    case "item.reasoning.textDelta": {
      const delta = String(raw.delta ?? "");
      if (!delta) return [];
      return [{ type: "thinking", timestamp, content: delta }];
    }

    case "item.reasoning.summaryPartAdded":
      return [];

    case "item.plan.delta": {
      const delta = String(raw.delta ?? "");
      if (!delta) return [];
      return [{
        type: "task",
        timestamp,
        tool: "EnterPlanMode",
        itemId: raw.itemId ? String(raw.itemId) : undefined,
        input: { plan: delta, status: "streaming" },
      }];
    }

    case "item.mcpToolCall.progress": {
      const progress = raw.progress ?? raw.message ?? raw.delta;
      if (!progress) return [];
      return [{
        type: "tool_result",
        timestamp,
        tool: String(raw.tool ?? "mcp"),
        itemId: raw.itemId ? String(raw.itemId) : undefined,
        input: { progress: true },
        output: typeof progress === "string" ? progress : JSON.stringify(progress),
      }];
    }

    // ── Approval requests (sandbox mode) ────────────────────────────
    case "item.commandExecution.requestApproval":
    case "item.fileChange.requestApproval": {
      const kind: NonNullable<AgentEvent["approval"]>["kind"] =
        type === "item.fileChange.requestApproval" ? "file" : "command";
      const requestId = String(raw.request_id ?? raw.requestId ?? raw.id ?? `${type}-${timestamp}`);
      const cmd = String(
        raw.command ?? (raw.params as Record<string, unknown> | undefined)?.command ?? "",
      );
      const filePath = String(
        raw.path ?? raw.file ?? (raw.params as Record<string, unknown> | undefined)?.path ?? "",
      );
      const summary = kind === "command"
        ? (cmd ? `Run command: ${cmd}` : "Run command outside sandbox")
        : (filePath ? `Write outside sandbox: ${filePath}` : "Write file outside sandbox");
      const detail = kind === "command" ? cmd : filePath;
      return [{
        type: "approval_request",
        timestamp,
        approval: {
          requestId,
          kind,
          summary,
          detail,
          blocking: true,
        },
      }];
    }

    case "serverRequest.resolved":
      return [];

    default:
      return [{ type: "generic", timestamp, input: raw }];
  }
}

/**
 * Parses a Codex item envelope (item.started / item.updated / item.completed)
 * into zero or more AgentEvents.
 */
export function parseCodexItem(
  raw: Record<string, unknown>,
  timestamp: number,
  envelope: "item.started" | "item.updated" | "item.completed",
  state: CodexParseState,
): AgentEvent[] {
  const item = raw.item as Record<string, unknown> | undefined;
  if (!item || typeof item !== "object") return [];

  const itemType = item.type as string | undefined;

  // Normalise camelCase item types to snake_case for uniform matching.
  const normType = itemType?.replace(/([A-Z])/g, "_$1").toLowerCase().replace(/^_/, "") ?? "";

  switch (normType || itemType) {
    case "agent_message":
    case "agentMessage":
      if (envelope === "item.completed") {
        const text = String(item.text ?? "");
        // If deltas have already accumulated this message (or a trimmed
        // version of it), the final item.completed envelope is redundant.
        // A duplicate can also arrive just after turn/completed, after the
        // streaming buffer has been cleared; in that case compare with the
        // last completed assistant message as well.
        // Use trimmed comparison to handle trailing whitespace differences.
        const matchesStreaming =
          !!text && (state.lastAgentMessage === text || state.lastAgentMessage.trim() === text.trim());
        const completed = state.lastCompletedAgentMessage ?? "";
        const matchesCompleted =
          state.turnOpen === false &&
          !!text &&
          !!completed &&
          (completed === text || completed.trim() === text.trim());
        if (matchesStreaming || matchesCompleted) {
          state.onDupTextCollision?.();
          return [];
        }
        if (text) state.lastAgentMessage = text;
        return text ? [{ type: "text", timestamp, content: text }] : [];
      }
      return [];

    case "reasoning":
      if (envelope === "item.completed") {
        const text = String(item.text ?? item.content ?? "");
        return text ? [{ type: "thinking", timestamp, content: text }] : [];
      }
      return [];

    case "command_execution":
    case "commandExecution": {
      // App-server wire format is camelCase (Rust serde rename_all = "camelCase"):
      //   aggregatedOutput, exitCode, durationMs, commandActions, processId.
      // We also accept snake_case for backward compatibility with older payloads
      // and existing test fixtures. See:
      //   https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/item.rs
      //   https://openai-codex.mintlify.app/api/items#commandexecution
      const command = String(item.command ?? "");
      const itemId = String(item.id ?? "");
      const commandActions = (item.commandActions ?? item.command_actions) as unknown;
      const durationMs = typeof item.durationMs === "number"
        ? item.durationMs
        : typeof item.duration_ms === "number" ? item.duration_ms : undefined;

      if (envelope === "item.started") {
        return [{
          type: "tool_use",
          timestamp,
          tool: "Bash",
          command,
          itemId,
          input: {
            command,
            cwd: item.cwd,
            itemId,
            status: "in_progress",
            commandActions,
          },
        }];
      }
      if (envelope === "item.completed") {
        const status = item.status as string | undefined;

        if (status === "declined") {
          return [{
            type: "tool_result",
            timestamp,
            tool: "Bash",
            itemId,
            input: { final: true, status, exitCode: undefined, durationMs },
            output: `Command declined by sandbox: ${command}`,
            error: `Command declined by sandbox`,
          }];
        }

        const aggregatedRaw = item.aggregatedOutput ?? item.aggregated_output;
        const aggregated = aggregatedRaw != null ? String(aggregatedRaw) : undefined;
        const stdout = aggregated ?? (item.stdout != null ? String(item.stdout) : undefined);
        const stderr = item.stderr != null ? String(item.stderr) : undefined;
        const exitCodeRaw = item.exitCode ?? item.exit_code;
        const exitCode = typeof exitCodeRaw === "number" ? exitCodeRaw : undefined;

        let output = stdout ?? "";
        if (stderr) output += (output ? "\n" : "") + stderr;
        if (exitCode !== undefined && exitCode !== 0) {
          output += (output ? "\n" : "") + `Exit code: ${exitCode}`;
        }
        if (typeof durationMs === "number") {
          output += (output ? "\n" : "") + `Duration: ${(durationMs / 1000).toFixed(1)}s`;
        }

        // Always provide a non-empty output string when the item completes,
        // so the renderer flips the card from "running" to "completed" even
        // when the command produced no stdout/stderr (e.g. a successful
        // `cd` or `mkdir`). Otherwise BashCard.tsx leaves `isRunning = true`
        // forever because `hasOutput` stays false.
        if (!output && status === "completed" && exitCode === 0) {
          output = "(no output)";
        }

        return [{
          type: "tool_result",
          timestamp,
          tool: "Bash",
          itemId,
          input: { final: true, status, exitCode, durationMs },
          output: output || undefined,
        }];
      }
      return [];
    }

    case "file_change":
    case "fileChange": {
      const itemStatus = (item.status as string | undefined) ?? "completed";
      const itemId = String(item.id ?? "");
      const changes = item.changes as Array<Record<string, unknown>> | undefined;

      if (envelope === "item.started") {
        // FileChange items don't carry a top-level path on the wire
        // (per the v2 protocol the path lives inside changes[i].path),
        // but older test fixtures sometimes include item.path / item.file.
        const firstChangePath = Array.isArray(changes) && changes.length > 0
          ? String(changes[0]?.path ?? "")
          : "";
        const path = String(item.path ?? item.file ?? firstChangePath);
        return [{
          type: "tool_use",
          timestamp,
          tool: "Write",
          input: { itemId, status: itemStatus, file_path: path },
        }];
      }
      if (envelope !== "item.completed") return [];

      if (Array.isArray(changes) && changes.length > 0) {
        return changes.map((change) => buildFileChangeEvent(change, itemStatus, itemId, timestamp));
      }
      return [buildFileChangeEvent(item, itemStatus, itemId, timestamp)];
    }

    case "mcp_tool_call":
    case "mcpToolCall": {
      const itemId = String(item.id ?? "");
      const server = String(item.server ?? "");
      const durationMs = typeof item.durationMs === "number"
        ? item.durationMs
        : typeof item.duration_ms === "number" ? item.duration_ms : undefined;
      if (envelope === "item.completed") {
        return [{
          type: "tool_result",
          timestamp,
          tool: String(item.tool ?? item.name ?? ""),
          itemId,
          input: { server, durationMs, status: item.status, final: true },
          output: item.result ?? item.output,
          error: item.error != null ? String(item.error) : undefined,
        }];
      }
      return [{
        type: "tool_use",
        timestamp,
        tool: String(item.tool ?? item.name ?? ""),
        itemId,
        input: {
          server,
          ...(item.arguments as Record<string, unknown> | undefined),
          arguments: item.arguments ?? item.input,
          status: item.status ?? "in_progress",
        },
      }];
    }

    case "web_search":
    case "webSearch":
      if (envelope === "item.completed") {
        const action = item.action as Record<string, unknown> | undefined;
        const itemId = String(item.id ?? "");
        return [{
          type: "web",
          timestamp,
          tool: "WebSearch",
          itemId,
          path: String(item.query ?? action?.query ?? action?.url ?? ""),
          input: {
            query: item.query,
            url: action?.url,
            actionType: action?.type,
            line: action?.line,
          },
        }];
      }
      return [];

    case "plan":
    case "plan_update":
    case "planUpdate":
      if (envelope === "item.completed") {
        return [{
          type: "task",
          timestamp,
          tool: "EnterPlanMode",
          itemId: String(item.id ?? ""),
          input: {
            plan: item.text ?? item.content,
            status: "completed",
          },
        }];
      }
      if (envelope === "item.started" || envelope === "item.updated") {
        return [{
          type: "task",
          timestamp,
          tool: "EnterPlanMode",
          itemId: String(item.id ?? ""),
          input: {
            plan: item.text ?? item.content ?? "",
            status: envelope === "item.started" ? "in_progress" : "updated",
          },
        }];
      }
      return [];

    case "image_view":
    case "imageView":
      if (envelope === "item.completed") {
        const path = String(item.path ?? "");
        return [{
          type: "tool_use",
          timestamp,
          tool: "ImageView",
          itemId: String(item.id ?? ""),
          path,
          input: { file_path: path, path },
        }];
      }
      return [];

    case "dynamic_tool_call":
    case "dynamicToolCall":
      if (envelope === "item.completed") {
        return [{
          type: "tool_result",
          timestamp,
          tool: String(item.tool ?? item.name ?? ""),
          output: item.output ?? item.result,
          error: item.error != null ? String(item.error) : undefined,
        }];
      }
      if (envelope === "item.started") {
        return [{
          type: "tool_use",
          timestamp,
          tool: String(item.tool ?? item.name ?? ""),
          input: item.arguments ?? item.input,
        }];
      }
      return [];

    case "todo_list":
    case "todoList":
      if (envelope === "item.completed") {
        return [{
          type: "task",
          timestamp,
          tool: "TodoWrite",
          path: Array.isArray(item.todos)
            ? `${(item.todos as unknown[]).length} items`
            : "",
          input: { todos: item.todos },
        }];
      }
      return [];

    case "entered_review_mode":
    case "enteredReviewMode": {
      if (envelope === "item.completed" || envelope === "item.started") {
        const review = String(item.review ?? "current changes");
        return [{
          type: "generic",
          timestamp,
          tool: "CodexReview",
          input: { action: "started", review },
        }];
      }
      return [];
    }

    case "exited_review_mode":
    case "exitedReviewMode": {
      if (envelope === "item.completed") {
        const review = String(item.review ?? "");
        if (review) {
          return [{ type: "text", timestamp, content: review }];
        }
      }
      return [];
    }

    case "context_compaction":
    case "contextCompaction":
      if (envelope === "item.completed") {
        return [{
          type: "generic",
          timestamp,
          tool: "ContextCompaction",
          input: { message: "Context compacted — conversation history was summarized" },
        }];
      }
      return [];

    case "user_message":
    case "userMessage":
      return [];

    default:
      if (envelope === "item.completed") {
        return [{ type: "generic", timestamp, input: item }];
      }
      return [];
  }
}

/**
 * Extracts token usage data from a Codex `usage` payload. Maps the
 * modern field names to our normalised `AgentEvent.usage`.
 *
 * Accepts both wire formats:
 *   - App-server (camelCase, current): `inputTokens`, `outputTokens`,
 *     `cachedInputTokens`, `reasoningOutputTokens`. See:
 *     https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/turn.rs
 *     (struct `Usage`, with `#[serde(rename_all = "camelCase")]`).
 *   - Legacy / exec-mode JSONL: `input_tokens`, `output_tokens`,
 *     `cached_input_tokens`, `reasoning_output_tokens`.
 *
 * `cachedInputTokens` maps to `cacheReadInputTokens`; Codex does not
 * expose cache-creation tokens separately, so that counter stays at 0.
 */
export function extractUsageEvent(usage: Record<string, unknown>): AgentEvent | null {
  const inputTokens =
    typeof usage.inputTokens === "number" ? usage.inputTokens :
    typeof usage.input_tokens === "number" ? usage.input_tokens :
    typeof usage.prompt_tokens === "number" ? usage.prompt_tokens : 0;
  const baseOutput =
    typeof usage.outputTokens === "number" ? usage.outputTokens :
    typeof usage.output_tokens === "number" ? usage.output_tokens :
    typeof usage.completion_tokens === "number" ? usage.completion_tokens : 0;
  const reasoningTokens =
    typeof usage.reasoningOutputTokens === "number" ? usage.reasoningOutputTokens :
    typeof usage.reasoning_output_tokens === "number" ? usage.reasoning_output_tokens : 0;
  const outputTokens = baseOutput + reasoningTokens;
  const cacheReadInputTokens =
    typeof usage.cachedInputTokens === "number" ? usage.cachedInputTokens :
    typeof usage.cached_input_tokens === "number" ? usage.cached_input_tokens : 0;

  if (inputTokens === 0 && outputTokens === 0 && cacheReadInputTokens === 0) {
    return null;
  }

  return {
    type: "usage",
    timestamp: Date.now(),
    usage: {
      inputTokens,
      outputTokens,
      cacheCreationInputTokens: 0,
      cacheReadInputTokens,
    },
  };
}
