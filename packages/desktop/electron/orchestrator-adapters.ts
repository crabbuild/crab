/**
 * Agent CLI adapters for the Crab Desktop Orchestrator.
 *
 * Provides a common interface for spawning and communicating with
 * external code agents (Claude Code, Codex, or custom binaries).
 * Each adapter normalises the CLI's NDJSON output into a unified
 * AgentEvent stream consumed by the orchestrator lifecycle manager.
 */

import { spawn, ChildProcess, execFile } from "node:child_process";
import * as readline from "node:readline";
import * as path from "node:path";
import { getExpandedEnv } from "./env/userShellEnv";

// ─── macOS Path Normalization ─────────────────────────────────────────────────

/**
 * Normalizes a path for macOS where /var and /tmp are symlinks to /private/var
 * and /private/tmp. Prepends `/private` to paths starting with `/var/` or `/tmp/`;
 * retains paths already starting with `/private/var/` or `/private/tmp/` unchanged;
 * leaves all other paths unchanged.
 */
export function normalizeMacPath(p: string): string {
  if (p.startsWith("/private/var/") || p.startsWith("/private/tmp/")) {
    return p;
  }
  if (p.startsWith("/var/") || p.startsWith("/tmp/")) {
    return "/private" + p;
  }
  return p;
}

/**
 * Returns an array of path variants covering both the `/private`-prefixed and
 * non-prefixed forms for a given path. Useful for matching paths that may appear
 * in either form on macOS.
 *
 * - Paths starting with `/var/` or `/tmp/`: returns `[p, "/private" + p]`
 * - Paths starting with `/private/var/` or `/private/tmp/`: returns `[p, p.slice("/private".length)]`
 * - All other paths: returns `[p]`
 */
export function pathVariants(p: string): string[] {
  if (p.startsWith("/private/var/") || p.startsWith("/private/tmp/")) {
    // Longer variant first to avoid partial matches in mid-string replacement.
    return [p, p.slice("/private".length)];
  }
  if (p.startsWith("/var/") || p.startsWith("/tmp/")) {
    // Longer variant (/private-prefixed) first to avoid partial matches.
    return ["/private" + p, p];
  }
  return [p];
}

// ─── AgentEvent ──────────────────────────────────────────────────────────────

export interface AgentEvent {
  type:
    | "text"
    | "tool_use"
    | "tool_result"
    | "file_edit"
    | "file_read"
    | "command_run"
    | "command"
    | "search"
    | "web"
    | "task"
    | "generic"
    | "thinking"
    | "error"
    | "usage"
    | "approval_request"
    | "thread_started"
    | "command_output"
    | "command_exited"
    | "filesystem_change"
    | "done";
  timestamp: number;
  content?: string;
  tool?: string;
  input?: unknown;
  output?: unknown;
  path?: string;
  diff?: string;
  command?: string;
  stdout?: string;
  stderr?: string;
  exitCode?: number;
  error?: string;
  /** Session identifier for command/exec sessions. Set on command_output and command_exited events. */
  commandSessionId?: string;
  /** Watch identifier for filesystem watch events. Set on filesystem_change events. */
  fsWatchId?: string;
  /** Kind of filesystem change: "created", "modified", or "deleted". */
  fsChangeKind?: string;
  /** Token usage data extracted from agent CLI events. */
  usage?: {
    inputTokens: number;
    outputTokens: number;
    cacheCreationInputTokens: number;
    cacheReadInputTokens: number;
  };
  /** Codex session/thread identifier (emitted on `thread.started`). The
   *  orchestrator persists this so subsequent iterations can resume the
   *  thread instead of starting a fresh one. */
  threadId?: string;
  /** Codex item identifier — present on tool_use/tool_result events that
   *  belong to a specific item (commandExecution, fileChange, mcpToolCall,
   *  etc.). Used by the renderer to route streaming output deltas to the
   *  correct card when multiple items are active concurrently. */
  itemId?: string;
  /** Approval request payload for sandbox-blocked actions. Only set on
   *  events of type "approval_request". */
  approval?: {
    requestId: string;
    /** "command" for shell, "file" for file write outside the sandbox,
     *  "patch" for patch application escalations, or a future kind. */
    kind: "command" | "file" | "patch" | "other";
    /** One-line summary suitable for the card header. */
    summary: string;
    /** Optional full preview (e.g. command body, diff) for the expanded card. */
    detail?: string;
    /** When true, denying this request is a hard stop (the agent cannot
     *  continue without it). When false, the agent has a documented
     *  fallback path. */
    blocking: boolean;
  };
}

// ─── AgentAdapter ────────────────────────────────────────────────────────────

export type EventListener = (event: AgentEvent) => void;
export type ExitListener = (code: number) => void;

export abstract class AgentAdapter {
  abstract readonly name: string;
  abstract readonly binary: string;

  protected proc: ChildProcess | null = null;
  protected eventListeners: Set<EventListener> = new Set();
  protected exitListeners: Set<ExitListener> = new Set();
  /** Guards against emitting a second `done` event from the exit handler
   *  when the protocol already delivered one (turn.completed / result). */
  protected doneEmitted = false;
  private preparation: AbortController | undefined;

  abstract spawn(prompt: string, cwd: string, env?: Record<string, string>): void | Promise<void>;

  // Environment discovery is shared; cancellation only revokes this launch.
  // A late environment reply must not start a cancelled or superseded process.
  protected async prepareEnvironment(extra?: Record<string, string>): Promise<Record<string, string> | undefined> {
    this.preparation?.abort();
    const owner = new AbortController();
    this.preparation = owner;
    let stop!: () => void;
    const cancelled = new Promise<undefined>(resolve => { stop = () => resolve(undefined); });
    owner.signal.addEventListener("abort", stop, { once: true });
    try {
      const env = await Promise.race([getExpandedEnv(extra), cancelled]);
      return owner.signal.aborted ? undefined : env;
    } catch (error) {
      if (!owner.signal.aborted) throw error;
      return undefined;
    } finally {
      owner.signal.removeEventListener("abort", stop);
      if (this.preparation === owner) this.preparation = undefined;
    }
  }

  protected cancelPreparation(): void { this.preparation?.abort(); }

  abstract sendMessage(text: string): void;

  /**
   * Optional: send a follow-up to a long-lived adapter without re-spawning.
   * The orchestrator calls this from `iterate()` when it detects the method
   * is present. Implemented by CodexAppServerAdapter only.
   */
  iterate?(prompt: string): void;

  /**
   * Optional: deliver the user's approval decision live over the wire.
   * The orchestrator calls this from `recordApproval()` when present.
   * The adapter maps Crab's vocabulary to the protocol's vocabulary.
   */
  respondApproval?(requestId: string, decision: "allow" | "allow_once" | "deny"): void;

  cancel(): void {
    this.cancelPreparation();
    if (this.proc) {
      this.proc.kill("SIGTERM");
      this.proc = null;
    }
  }

  onEvent(listener: EventListener): () => void {
    this.eventListeners.add(listener);
    return () => {
      this.eventListeners.delete(listener);
    };
  }

  onExit(listener: ExitListener): () => void {
    this.exitListeners.add(listener);
    return () => {
      this.exitListeners.delete(listener);
    };
  }

  protected emitEvent(event: AgentEvent): void {
    if (event.type === "done") {
      this.doneEmitted = true;
    }
    for (const listener of this.eventListeners) {
      listener(event);
    }
  }

  protected emitExit(code: number): void {
    for (const listener of this.exitListeners) {
      listener(code);
    }
  }

  protected setupExitHandler(): void {
    if (!this.proc) return;
    this.doneEmitted = false;
    this.proc.on("exit", (code) => {
      // Emit a done event so the renderer stops the spinner immediately,
      // but only if the protocol stream didn't already deliver one
      // (turn.completed / result). Emitting twice causes the renderer
      // to split one turn into two, duplicating the summary card.
      if (!this.doneEmitted) {
        this.emitEvent({
          type: "done",
          timestamp: Date.now(),
          content: "",
        });
      }
      this.emitExit(code ?? 1);
      this.proc = null;
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
  }
}

// ─── ClaudeCodeAdapter ───────────────────────────────────────────────────────

/**
 * Adapter for Claude Code CLI.
 *
 * Spawns `claude --json --print --allowedTools Read,Write,Bash <prompt>`
 * and parses NDJSON from stdout into normalised AgentEvent objects.
 */
export class ClaudeCodeAdapter extends AgentAdapter {
  readonly name = "Claude Code";
  readonly binary = "claude";

  /** The working directory (worktree path) passed to spawn — used to strip absolute paths. */
  private cwd: string = "";
  /** Extra CLI arguments passed from per-agent config. */
  private extraArgs: string[] = [];

  constructor(extraArgs?: string[]) {
    super();
    this.extraArgs = extraArgs ?? [];
  }

  async spawn(prompt: string, cwd: string, env?: Record<string, string>): Promise<void> {
    const environment = await this.prepareEnvironment(env);
    if (!environment) return;
    this.cwd = cwd;
    this.proc = spawn(
      this.binary,
      [
        "--print",
        "--verbose",
        "--output-format", "stream-json",
        "--permission-mode", "bypassPermissions",
        ...this.extraArgs,
        prompt,
      ],
      {
        cwd,
        env: environment,
        stdio: ["pipe", "pipe", "pipe"],
      },
    );

    // Close stdin immediately to prevent the "no stdin data received" warning.
    // Claude Code waits 3s for stdin input when piped — closing it signals
    // that no input is coming.
    this.proc.stdin?.end();

    const rl = readline.createInterface({ input: this.proc.stdout! });
    rl.on("line", (line) => {
      if (!line.trim()) return;
      try {
        const raw = JSON.parse(line);
        const events = this.parseClaudeEvents(raw);
        for (const event of events) {
          if (event.type === "text" && !event.content) continue; // skip empty text
          this.emitEvent(event);
        }

        // If the event carries a usage field, emit a separate usage event
        // so the cost tracker can record token consumption.
        if (raw.usage && typeof raw.usage === "object") {
          const usageEvent = this.extractUsageEvent(raw.usage);
          if (usageEvent) {
            this.emitEvent(usageEvent);
          }
        }
      } catch {
        // Malformed line — skip silently.
      }
    });

    this.proc.stderr?.on("data", (chunk: Buffer) => {
      const text = chunk.toString().trim();
      if (!text) return;
      this.emitEvent({
        type: "error",
        timestamp: Date.now(),
        error: text,
      });
    });

    this.setupExitHandler();
  }

  sendMessage(text: string): void {
    if (this.proc?.stdin?.writable) {
      this.proc.stdin.write(JSON.stringify({ type: "user", content: text }) + "\n");
    }
  }

  /**
   * Maps Claude Code's NDJSON event format to our normalised AgentEvent.
   *
   * Claude Code emits events with a `type` field. Known types:
   * - assistant: text output from the model
   * - tool_use: agent is calling a tool
   * - tool_result: result from a tool call
   * - thinking: model reasoning
   * - error: an error occurred
   * - result: final result (maps to "done")
   */

  /** Convenience wrapper: parses a single raw event and returns the first AgentEvent. */
  parseClaudeEvent(raw: Record<string, unknown>): AgentEvent {
    const events = this.parseClaudeEvents(raw);
    return events[0] ?? { type: "text", timestamp: Date.now(), content: "" };
  }

  parseClaudeEvents(raw: Record<string, unknown>): AgentEvent[] {
    const timestamp = Date.now();
    const type = raw.type as string | undefined;

    // Skip system/init events.
    if (type === "system") {
      return [];
    }

    // Assistant message with content blocks.
    if (type === "assistant") {
      const message = raw.message as Record<string, unknown> | undefined;
      if (message) {
        const content = message.content as Array<Record<string, unknown>> | undefined;
        if (Array.isArray(content)) {
          const events: AgentEvent[] = [];
          const textParts: string[] = [];

          for (const block of content) {
            if (block.type === "text") {
              textParts.push(String(block.text ?? ""));
            } else if (block.type === "tool_use") {
              // Flush text before tool event.
              if (textParts.length > 0) {
                events.push({ type: "text", timestamp, content: textParts.join("\n") });
                textParts.length = 0;
              }
              events.push(this.parseClaudeToolUse(block, timestamp));
            } else if (block.type === "thinking") {
              events.push({ type: "thinking", timestamp, content: String(block.thinking ?? block.text ?? "") });
            }
          }
          if (textParts.length > 0) {
            events.push({ type: "text", timestamp, content: textParts.join("\n") });
          }
          return events;
        }
        // Fallback: try raw content/text fields.
        const rawContent = message.content ?? message.text;
        if (typeof rawContent === "string" && rawContent) {
          return [{ type: "text", timestamp, content: rawContent }];
        }
      }
      // Legacy format: direct content field.
      if (raw.content || raw.message) {
        const c = String(raw.content ?? raw.message ?? "");
        return c ? [{ type: "text", timestamp, content: c }] : [];
      }
      return [];
    }

    // Tool use event (top-level).
    if (type === "tool_use") {
      return [this.parseClaudeToolUse(raw, timestamp)];
    }

    // Tool result event (top-level).
    if (type === "tool_result") {
      return [{
        type: "tool_result",
        timestamp,
        tool: String(raw.tool ?? raw.name ?? raw.tool_use_id ?? ""),
        output: raw.output ?? raw.content,
      }];
    }

    // Tool result / user message with tool results.
    if (type === "user") {
      const message = raw.message as Record<string, unknown> | undefined;
      if (message) {
        const content = message.content as Array<Record<string, unknown>> | undefined;
        if (Array.isArray(content)) {
          const events: AgentEvent[] = [];
          for (const block of content) {
            if (block.type === "tool_result") {
              events.push({
                type: "tool_result",
                timestamp,
                tool: String(block.tool_use_id ?? ""),
                output: block.content,
              });
            }
          }
          return events;
        }
      }
      return [];
    }

    // Result event (task completed).
    if (type === "result") {
      const subResult = raw.subtype as string | undefined;
      if (subResult === "error") {
        return [{ type: "error", timestamp, error: String(raw.error ?? raw.message ?? "Task failed") }];
      }
      return [{ type: "done", timestamp, content: String(raw.result ?? raw.content ?? "") }];
    }

    // Thinking event.
    if (type === "thinking") {
      const c = String(raw.content ?? raw.thinking ?? "");
      return c ? [{ type: "thinking", timestamp, content: c }] : [];
    }

    // Error event.
    if (type === "error") {
      return [{ type: "error", timestamp, error: String(raw.error ?? raw.message ?? "unknown error") }];
    }

    // Skip empty/unrecognized events silently.
    return [];
  }

  private parseClaudeToolUse(
    raw: Record<string, unknown>,
    timestamp: number,
  ): AgentEvent {
    const tool = String(raw.tool ?? raw.name ?? "");
    const input = raw.input as Record<string, unknown> | undefined;

    // Map specific tool uses to more specific event types.
    if (tool === "Write" || tool === "Edit" || tool === "NotebookEdit") {
      // Strip absolute worktree path to show relative path only.
      const rawPath = String(input?.file_path ?? input?.path ?? "");
      const cleanPath = this.stripWorktreePath(rawPath);
      return {
        type: "file_edit",
        timestamp,
        tool,
        path: cleanPath,
        diff: String(input?.content ?? input?.diff ?? input?.new_string ?? ""),
        input,
      };
    }

    if (tool === "Read") {
      const rawPath = String(input?.file_path ?? input?.path ?? "");
      const cleanPath = this.stripWorktreePath(rawPath);
      return {
        type: "file_read",
        timestamp,
        tool,
        path: cleanPath,
        input,
      };
    }

    if (tool === "Grep") {
      const pattern = String(input?.pattern ?? input?.regex ?? "");
      const rawPath = String(input?.path ?? input?.directory ?? "");
      const cleanPath = this.stripWorktreePath(rawPath);
      return {
        type: "search",
        timestamp,
        tool,
        path: cleanPath || pattern,
        input,
      };
    }

    if (tool === "Glob") {
      const pattern = String(input?.pattern ?? input?.glob ?? "");
      const rawPath = String(input?.path ?? "");
      const cleanPath = this.stripWorktreePath(rawPath);
      return {
        type: "search",
        timestamp,
        tool,
        path: cleanPath || pattern,
        input,
      };
    }

    if (tool === "WebFetch") {
      const url = String(input?.url ?? "");
      return {
        type: "web",
        timestamp,
        tool,
        path: url,
        input,
      };
    }

    if (tool === "Bash" || tool === "Terminal") {
      // Strip worktree paths from the command string.
      const rawCmd = String(input?.command ?? "");
      const cleanCmd = this.stripWorktreePath(rawCmd);
      return {
        type: "command",
        timestamp,
        tool,
        command: cleanCmd,
        input,
      };
    }

    if (tool === "TodoWrite") {
      return {
        type: "task",
        timestamp,
        tool,
        path: String(input?.todos ? `${(input.todos as unknown[]).length} items` : ""),
        input,
      };
    }

    // Sub-agent tools
    if (tool === "Task" || tool === "TaskOutput" || tool === "TaskStop") {
      return {
        type: "task",
        timestamp,
        tool,
        path: String(input?.description ?? input?.task_id ?? ""),
        input,
      };
    }

    // All other tools — emit as "generic" with tool name preserved.
    return {
      type: "generic",
      timestamp,
      tool,
      input,
    };
  }

  /**
   * Strips the worktree absolute path prefix from a string, leaving
   * only the relative path. Cross-platform: uses the cwd passed to
   * spawn() and handles path normalization.
   *
   * Uses `pathVariants` to cover both `/var/folders/...` and
   * `/private/var/folders/...` forms regardless of which form the
   * stored cwd uses.
   */
  private stripWorktreePath(value: string): string {
    if (!value || !this.cwd) return value;

    const variants = pathVariants(this.cwd);

    // Check if the value is a pure path that starts with any variant.
    const normalizedValue = this.normalizePath(value);
    for (const variant of variants) {
      if (normalizedValue.startsWith(variant + "/")) {
        return normalizedValue.slice(variant.length + 1);
      }
      if (normalizedValue === variant) {
        return "";
      }
    }

    // Mid-string replacement: strip all occurrences of each variant + "/"
    // from the value (e.g. command strings containing embedded paths).
    for (const variant of variants) {
      if (value.includes(variant + "/")) {
        value = value.replaceAll(variant + "/", "");
      }
    }

    return value;
  }

  /**
   * Normalizes a path by resolving the /private symlink on macOS.
   * On macOS, /var is a symlink to /private/var, so paths may appear
   * with or without the /private prefix depending on how they were resolved.
   * On other platforms, this is a no-op.
   */
  private normalizePath(p: string): string {
    return normalizeMacPath(p);
  }

  /**
   * Extracts token usage data from a Claude Code event's `usage` field
   * and returns a normalised `AgentEvent` with type `"usage"`.
   */
  extractUsageEvent(usage: Record<string, unknown>): AgentEvent | null {
    const inputTokens = typeof usage.input_tokens === "number" ? usage.input_tokens : 0;
    const outputTokens = typeof usage.output_tokens === "number" ? usage.output_tokens : 0;
    const cacheCreationInputTokens =
      typeof usage.cache_creation_input_tokens === "number" ? usage.cache_creation_input_tokens : 0;
    const cacheReadInputTokens =
      typeof usage.cache_read_input_tokens === "number" ? usage.cache_read_input_tokens : 0;

    // Skip if all zeros (no meaningful usage data).
    if (inputTokens === 0 && outputTokens === 0 && cacheCreationInputTokens === 0 && cacheReadInputTokens === 0) {
      return null;
    }

    return {
      type: "usage",
      timestamp: Date.now(),
      usage: {
        inputTokens,
        outputTokens,
        cacheCreationInputTokens,
        cacheReadInputTokens,
      },
    };
  }
}

// ─── GenericAdapter ──────────────────────────────────────────────────────────

/**
 * Adapter for user-configured custom agent binaries.
 *
 * Expects the binary to produce NDJSON matching the AgentEvent schema
 * directly — no mapping needed. The binary receives the prompt as the
 * first positional argument.
 */
export class GenericAdapter extends AgentAdapter {
  readonly name: string;
  readonly binary: string;
  private readonly args: string[];

  constructor(binary: string, name?: string, extraArgs?: string[]) {
    super();
    this.binary = binary;
    this.name = name ?? path.basename(binary);
    this.args = extraArgs ?? [];
  }

  async spawn(prompt: string, cwd: string, env?: Record<string, string>): Promise<void> {
    const environment = await this.prepareEnvironment(env);
    if (!environment) return;
    this.proc = spawn(this.binary, [...this.args, prompt], {
      cwd,
      env: environment,
      stdio: ["pipe", "pipe", "pipe"],
    });

    const rl = readline.createInterface({ input: this.proc.stdout! });
    rl.on("line", (line) => {
      if (!line.trim()) return;
      try {
        const raw = JSON.parse(line) as AgentEvent;
        // Ensure timestamp is present.
        if (!raw.timestamp) {
          raw.timestamp = Date.now();
        }
        this.emitEvent(raw);
      } catch {
        // Malformed line — skip silently.
      }
    });

    this.proc.stderr?.on("data", (chunk: Buffer) => {
      const text = chunk.toString().trim();
      if (text) {
        this.emitEvent({
          type: "error",
          timestamp: Date.now(),
          error: text,
        });
      }
    });

    this.setupExitHandler();
  }

  sendMessage(text: string): void {
    if (this.proc?.stdin?.writable) {
      this.proc.stdin.write(JSON.stringify({ type: "user", content: text }) + "\n");
    }
  }
}

// ─── Agent Detection Utility ─────────────────────────────────────────────────

export interface DetectedAgent {
  /** Registry identifier, e.g. "claude-code" / "codex" / "gemini-cli" / "custom". */
  type: string;
  /** Display name, e.g. "Claude Code" / "Codex". */
  name: string;
  binary: string;
  path: string;
  version: string;
  available: boolean;
  /**
   * When true, this agent should be spawned in an interactive terminal
   * rather than through the headless orchestrator adapter. The orchestrator
   * input box serializes context into CLI arguments and opens a terminal tab.
   */
  terminalOnly?: boolean;
}

// ─── Detection cache + prewarm ───────────────────────────────────────────────
//
// `detectAgents()` used to be expensive: a sequential loop that spawned
// `which` (and a login shell fallback) plus a `--version` call for every
// registered agent. On a cold start with 5–7 agents and a heavy `.zshrc`
// that added up to 10–30 s, blocking the orchestrator view's initial render.
//
// We fix that two ways:
//   1. Parallelize the per-agent probes — total latency is now max(probe)
//      rather than sum.
//   2. Cache the result in-process with a short TTL and dedup concurrent
//      calls, so repeat opens are essentially free. `prewarmAgents()` is
//      called from `main.ts` at startup so the first renderer request
//      usually hits a warm cache.

const DETECT_CACHE_TTL_MS = 5 * 60 * 1000; // 5 minutes

interface DetectCacheEntry {
  agents: DetectedAgent[];
  timestamp: number;
}

let _detectCache: DetectCacheEntry | null = null;
let _detectInFlight: Promise<DetectedAgent[]> | null = null;

export interface DetectAgentsOptions {
  /** Skip cache and probe the system fresh. */
  force?: boolean;
}

/**
 * Scans PATH for known agent CLIs and returns version info for each.
 *
 * Cached for {@link DETECT_CACHE_TTL_MS}; pass `{ force: true }` to
 * bypass the cache (used by the "Refresh" button in Settings). Concurrent
 * callers share a single in-flight probe.
 */
export async function detectAgents(options?: DetectAgentsOptions): Promise<DetectedAgent[]> {
  const now = Date.now();
  if (!options?.force && _detectCache && now - _detectCache.timestamp < DETECT_CACHE_TTL_MS) {
    return _detectCache.agents;
  }
  if (_detectInFlight) return _detectInFlight;

  _detectInFlight = probeAgents()
    .then((result) => {
      _detectCache = { agents: result, timestamp: Date.now() };
      return result;
    })
    .finally(() => {
      _detectInFlight = null;
    });

  return _detectInFlight;
}

/** Returns the cached agent list without probing. `null` if unseeded. */
export function getCachedAgents(): DetectedAgent[] | null {
  return _detectCache?.agents ?? null;
}

/**
 * Kicks off a background probe without awaiting. Intended to be called
 * once at electron startup so the first renderer call hits a warm cache.
 * Swallows errors — detection failures show up as `available: false` in
 * the UI and shouldn't crash the app.
 */
export function prewarmAgents(): void {
  detectAgents().catch(() => {});
}

async function probeAgents(): Promise<DetectedAgent[]> {
  if (!_registry) {
    throw new Error("Agent registry not initialized. Ensure orchestrator-registry is imported.");
  }

  const registrations = _registry.listRegistered();

  // Lookup table for built-in types that don't have a manifest
  const builtinBinaryMap: Record<string, string> = {
    "claude-code": "claude",
    "codex": "codex",
  };

  // Probe in parallel — each detectBinaryWithFlag may spawn `which` plus a
  // login-shell fallback and a --version call, each with its own timeout.
  // Running them serially means the sum of timeouts; in parallel it's the
  // slowest single probe.
  const results = await Promise.all(
    registrations.map((reg) => {
      const binary = reg.manifest?.binary ?? builtinBinaryMap[reg.type] ?? reg.type;
      const versionFlag = reg.manifest?.versionFlag ?? "--version";
      return detectBinaryWithFlag(reg.type, binary, reg.name, versionFlag);
    }),
  );

  // Enrich with terminalOnly from capabilities.
  for (let i = 0; i < results.length; i++) {
    const reg = registrations[i];
    if (reg.capabilities.terminalOnly) {
      results[i].terminalOnly = true;
    }
  }

  return results;
}

/**
 * Detects a single binary on PATH and retrieves its version using a custom flag.
 */
async function detectBinaryWithFlag(
  type: string,
  binary: string,
  name: string,
  versionFlag: string,
): Promise<DetectedAgent> {
  const binaryPath = await findOnPath(binary);
  if (!binaryPath) {
    return { type, name, binary, path: "", version: "", available: false };
  }

  const version = await getVersion(binaryPath, versionFlag);
  return { type, name, binary, path: binaryPath, version, available: true };
}

// Detection and process launches share the resolved login environment, so GUI
// startup never needs a second synchronous shell probe.
async function findOnPath(binary: string): Promise<string | null> {
  const env = await getExpandedEnv();
  const command = process.platform === "win32" ? "where" : "which";
  return new Promise(resolve => {
    execFile(command, [binary], { timeout: 5000, env }, (error, stdout) => {
      resolve(error || !stdout.trim() ? null : stdout.trim().split(/\r?\n/)[0].trim());
    });
  });
}

/**
 * Runs `<binary> <versionFlag>` and returns the version string.
 */
function getVersion(binaryPath: string, versionFlag: string = "--version"): Promise<string> {
  return new Promise((resolve) => {
    execFile(binaryPath, [versionFlag], { timeout: 5000 }, (err, stdout, stderr) => {
      if (err) {
        resolve("");
        return;
      }
      // Version output may be on stdout or stderr.
      const output = (stdout || stderr).trim();
      // Extract version number from output like "claude 1.2.3" or "codex v0.1.0".
      const match = output.match(/(\d+\.\d+[\w.-]*)/);
      resolve(match ? match[1] : output.split("\n")[0]);
    });
  });
}

// ─── Adapter Factory ─────────────────────────────────────────────────────────

export type AgentType = string;

/**
 * Registry reference, set by orchestrator-registry.ts after it initializes.
 * This breaks the circular dependency: adapters exports classes that registry
 * imports, and registry sets itself here so adapters can delegate to it.
 */
interface RegistryLike {
  createAdapter(type: string, config?: {
    binaryPath?: string;
    extraArgs?: string[];
    env?: Record<string, string>;
    optOutNotifications?: string[];
  }): AgentAdapter;
  listRegistered(): Array<{
    type: string;
    name: string;
    manifest?: { binary?: string; versionFlag?: string };
    capabilities: { terminalOnly?: boolean };
  }>;
}

let _registry: RegistryLike | null = null;

/** Called by orchestrator-registry.ts to inject the registry reference. */
export function _setRegistry(registry: RegistryLike): void {
  _registry = registry;
}

/**
 * Creates the appropriate adapter for the given agent type.
 * Delegates to the agent registry. The registry must be initialized
 * before this function is called.
 *
 * Legacy signature (for backward compatibility):
 *   createAdapter(type, customBinary?, customName?, customArgs?)
 *
 * New callers should pass a config object directly via the registry.
 */
export function createAdapter(
  agentType: AgentType,
  customBinary?: string,
  _customName?: string,
  customArgs?: string[],
  config?: { env?: Record<string, string>; optOutNotifications?: string[] },
): AgentAdapter {
  if (!_registry) {
    throw new Error("Agent registry not initialized. Ensure orchestrator-registry is imported.");
  }

  if (agentType === "custom") {
    if (!customBinary) {
      throw new Error("customBinary is required for custom agent type");
    }
    return _registry.createAdapter(agentType, {
      binaryPath: customBinary,
      extraArgs: customArgs,
      env: config?.env,
      optOutNotifications: config?.optOutNotifications,
    });
  }
  return _registry.createAdapter(agentType, {
    binaryPath: customBinary,
    extraArgs: customArgs,
    env: config?.env,
    optOutNotifications: config?.optOutNotifications,
  });
}
