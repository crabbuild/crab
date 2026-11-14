/**
 * Shared Codex parsing utilities.
 *
 * Maps official app-server items to the shared agent event contract.
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

// ─── Shared State Interface ──────────────────────────────────────────────────

export interface CodexParseState {
  agentMessages: Map<string, string>;
  lastAgentMessage: string;
}

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

/** Parse the item notifications used by the app-server adapter. */
export function parseCodexEvents(raw: Record<string, unknown>, state: CodexParseState): AgentEvent[] {
  const params = raw.params as Record<string, unknown> | undefined;
  if (!params) return [];
  const timestamp = Date.now();
  if (raw.method === "item/agentMessage/delta") {
    const itemId = String(params.itemId);
    const delta = String(params.delta ?? "");
    if (!delta) return [];
    const text = (state.agentMessages.get(itemId) ?? "") + delta;
    state.agentMessages.set(itemId, text);
    state.lastAgentMessage = text;
    return [{ type: "text", timestamp, content: delta, itemId }];
  }
  if (raw.method === "item/started" || raw.method === "item/completed") {
    return parseCodexItem(params, timestamp, raw.method === "item/started" ? "item.started" : "item.completed", state);
  }
  return [];
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
        const itemId = String(item.id);
        const previous = state.agentMessages.get(itemId);
        state.agentMessages.set(itemId, text);
        state.lastAgentMessage = text;
        // Completion is the authoritative snapshot for this item. Matching
        // text on another item must not suppress a distinct assistant message.
        if (previous === text) return [];
        return [{ type: "text", timestamp, content: text, itemId, textMode: "replace" }];
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
          itemId: String(item.id),
          input: { final: true },
          output: item.contentItems ?? item.output ?? item.result,
          error: item.success === false ? "Tool call failed" : item.error != null ? String(item.error) : undefined,
        }];
      }
      if (envelope === "item.started") {
        return [{
          type: "tool_use",
          timestamp,
          tool: String(item.tool ?? item.name ?? ""),
          itemId: String(item.id),
          input: item.arguments ?? item.input,
        }];
      }
      return [];

    case "collab_agent_tool_call": {
      const tool = String(item.tool);
      const itemId = String(item.id);
      if (envelope === "item.started") return [{ type: "task", timestamp, tool: "CodexAgent", itemId, input: item }];
      if (envelope !== "item.completed") return [];
      const result: AgentEvent[] = [{ type: "tool_result", timestamp, itemId, input: { final: true }, output: JSON.stringify(item.agentsStates ?? {}),
        error: item.status === "failed" ? "Agent operation failed" : undefined }];
      if (["sendInput", "sendMessage", "followupTask"].includes(tool) && item.prompt && Array.isArray(item.receiverThreadIds)) {
        for (const to of item.receiverThreadIds) result.push({ type: "agent_message", timestamp, itemId,
          agentMessage: { from: String(item.senderThreadId), to: String(to), content: String(item.prompt), delivery: item.status === "completed" ? "accepted" : "failed" } });
      }
      return result;
    }

    case "sub_agent_activity":
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
      if (envelope === "item.started") {
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

/** Normalize a request's app-server usage into disjoint billing categories. */
export function extractUsageEvent(usage: Record<string, unknown>): AgentEvent | null {
  const cacheReadInputTokens = Number(usage.cachedInputTokens ?? 0);
  const cacheCreationInputTokens = Number(usage.cacheWriteInputTokens ?? 0);
  // Responses includes cached input in inputTokens and reasoning in outputTokens.
  // The cost tracker prices cache categories separately; subtract them once.
  const inputTokens = Math.max(0, Number(usage.inputTokens ?? 0) - cacheReadInputTokens - cacheCreationInputTokens);
  const outputTokens = Number(usage.outputTokens ?? 0);
  if (!inputTokens && !outputTokens && !cacheReadInputTokens && !cacheCreationInputTokens) return null;
  return { type: "usage", timestamp: Date.now(), usage: {
    inputTokens, outputTokens, cacheCreationInputTokens, cacheReadInputTokens,
  } };
}
