/**
 * Agent CLI adapters for the Crab Desktop Orchestrator.
 *
 * Provides a common interface for spawning and communicating with
 * external code agents (Claude Code, Codex, or custom binaries).
 * Each adapter normalises the CLI's NDJSON output into a unified
 * AgentEvent stream consumed by the orchestrator lifecycle manager.
 */

import type { AgentElicitationInput, ElicitationResponse } from "./agent-elicitation";
import { spawn, execFile } from "node:child_process";
import type { AgentProcess, AgentProcessFactory } from "./agent-process";
import * as readline from "node:readline";
import * as path from "node:path";
import * as fs from "node:fs/promises";
import { constants } from "node:fs";
import { getExpandedEnv } from "./env/userShellEnv";
import { readSettings, type StoredAgentConfig } from "./orchestrator-settings";

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

export interface AgentModel {
  value: string;
  label: string;
  description?: string;
  intelligenceLevels: string[];
  defaultIntelligence?: string;
  isDefault?: boolean;
}

/** Omission retains current settings; null restores provider defaults. */
export interface AgentTurnOptions {
  model?: string | null;
  mode?: "plan" | "instant";
  reasoningLevel?: string | null;
}

export interface AgentQuestionInput {
  kind?: "questions";
  requestId: string;
  questions: Array<{
    id: string;
    question: string;
    header?: string;
    options?: Array<{ label: string; description?: string }>;
    multiSelect?: boolean;
    isSecret?: boolean;
    allowFreeform?: boolean;
  }>;
  resolved?: boolean;
  resolutionReason?: string;
}

export type AgentInputRequest = AgentQuestionInput | AgentElicitationInput;
export type AgentInputResponse = { kind: "questions"; answers: Record<string, string[]> } | ElicitationResponse;

export interface CodexGoal {
  threadId: string;
  objective: string;
  status: "active" | "paused" | "blocked" | "usageLimited" | "budgetLimited" | "complete";
  tokenBudget: number | null;
  tokensUsed: number;
  timeUsedSeconds: number;
  createdAt: number;
  updatedAt: number;
}

export interface CodexGoalMutation {
  objective?: string;
  status?: CodexGoal["status"];
  tokenBudget?: number;
}

export interface AgentEvent {
  type:
    | "text"
    | "user_message"
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
    | "input_request"
    | "request_resolved"
    | "thread_started"
    | "turn_started"
    | "session_started"
    | "configuration_changed"
    | "goal_updated"
    | "command_output"
    | "command_exited"
    | "filesystem_change"
    | "done";
  timestamp: number;
  goal?: CodexGoal | null;
  content?: string;
  /** A completed text block replaces its partial stream, keyed by itemId. */
  textMode?: "replace";
  tool?: string;
  input?: unknown;
  output?: unknown;
  path?: string;
  diff?: string;
  command?: string;
  stdout?: string;
  stderr?: string;
  exitCode?: number;
  outcome?: "completed" | "failed" | "interrupted";
  turnId?: string;
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
    model?: string;
    /** Whole-session provider totals, including subagents, scoped by model. */
    cumulative?: { sessionId: string; costUsd: number };
  };
  /** Codex session/thread identifier (emitted on `thread.started`). The
   *  orchestrator persists this so subsequent iterations can resume the
   *  thread instead of starting a fresh one. */
  threadId?: string;
  /** Claude Code session identity, persisted for continuation after restart. */
  sessionId?: string;
  configuration?: AgentTurnOptions;
  inputRequest?: AgentInputRequest;
  requestId?: string;
  /** Codex item identifier — present on tool_use/tool_result events that
   *  belong to a specific item (commandExecution, fileChange, mcpToolCall,
   *  etc.). Used by the renderer to route streaming output deltas to the
   *  correct card when multiple items are active concurrently. */
  itemId?: string;
  /** Approval request payload for sandbox-blocked actions. Only set on
   *  events of type "approval_request". */
  approval?: {
    requestId: string;
    resolved?: boolean;
    resolutionReason?: string;
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
    options?: Array<{ id: string; label: string }>;
  };
}

// ─── AgentAdapter ────────────────────────────────────────────────────────────

export type EventListener = (event: AgentEvent) => void;
export type ExitListener = (code: number) => void;

/** A dispatched message may have reached the provider before its acknowledgement was lost. */
export class AgentSubmissionUncertainError extends Error {
  constructor(cause: unknown) {
    const detail = cause instanceof Error ? cause.message : String(cause);
    super(`Message delivery could not be confirmed: ${detail}`, { cause });
    this.name = "AgentSubmissionUncertainError";
  }
}

export abstract class AgentAdapter {
  abstract readonly name: string;
  abstract readonly binary: string;

  protected proc: AgentProcess | null = null;
  protected eventListeners: Set<EventListener> = new Set();
  protected exitListeners: Set<ExitListener> = new Set();
  /** Guards against emitting a second `done` event from the exit handler
   *  when the protocol already delivered one (turn.completed / result). */
  protected doneEmitted = false;
  private preparation: AbortController | undefined;

  protected exitEmitted = false;

  abstract spawn(prompt: string, cwd: string, env?: Record<string, string>, options?: AgentTurnOptions): void | Promise<void>;

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

  /** Read the installed provider's catalog without submitting a user turn. */
  listModels?(cwd: string, env?: Record<string, string>): Promise<AgentModel[]>;

  /**
   * Optional: send a follow-up to a long-lived adapter without re-spawning.
   * The orchestrator calls this from `iterate()` when it detects the method
   * is present. Both built-in persistent adapters implement it.
   */
  iterate?(prompt: string, options?: AgentTurnOptions): void | Promise<void>;

  /** Runs a native review, opening the provider connection when needed. */
  review?(instructions: string, cwd: string, env?: Record<string, string>, options?: AgentTurnOptions): Promise<void>;

  /**
   * Optional: deliver the user's approval decision live over the wire.
   * The orchestrator calls this from `recordApproval()` when present.
   * The option id must be one of the decisions offered with the request.
   */
  respondApproval?(requestId: string, decision: string): boolean;
  respondInput?(requestId: string, response: AgentInputResponse): boolean;

  cancel(): void | Promise<void> {
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
    if (this.exitEmitted) return;
    this.exitEmitted = true;
    for (const listener of this.exitListeners) {
      listener(code);
    }
  }

  protected setupExitHandler(): void {
    if (!this.proc) return;
    this.doneEmitted = false;
    this.exitEmitted = false;
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
  /** An enabled executable was explicitly configured in settings or a manifest. */
  configured?: boolean;
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
  settingsKey: string;
}

let _detectCache: DetectCacheEntry | null = null;
let _detectInFlight: Promise<DetectedAgent[]> | null = null;
let _detectInFlightKey = "";

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
  const configs = readSettings().orchestratorAgentConfigs;
  const settingsKey = JSON.stringify(configs);
  const now = Date.now();
  if (!options?.force && _detectCache?.settingsKey === settingsKey && now - _detectCache.timestamp < DETECT_CACHE_TTL_MS) {
    return _detectCache.agents;
  }
  if (_detectInFlight && _detectInFlightKey === settingsKey) return _detectInFlight;

  const pending = probeAgents(configs)
    .then((result) => {
      if (_detectInFlight === pending) _detectCache = { agents: result, timestamp: Date.now(), settingsKey };
      return result;
    })
    .finally(() => {
      if (_detectInFlight === pending) _detectInFlight = null;
    });
  _detectInFlight = pending;
  _detectInFlightKey = settingsKey;
  return pending;
}

/** Returns the cached agent list without probing. `null` if unseeded. */
export function getCachedAgents(): DetectedAgent[] | null {
  return _detectCache?.settingsKey === JSON.stringify(readSettings().orchestratorAgentConfigs)
    ? _detectCache.agents : null;
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

async function probeAgents(configs: Record<string, unknown>): Promise<DetectedAgent[]> {
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
      const config = (configs[reg.type] ?? {}) as StoredAgentConfig;
      return detectBinaryWithFlag(reg.type, binary, reg.name, versionFlag, config);
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
  config: StoredAgentConfig,
): Promise<DetectedAgent> {
  if (config.enabled === false) return { type, name, binary, path: "", version: "", available: false };
  const configuredPath = config.binaryPath?.trim();
  const configured = configuredPath || path.isAbsolute(binary) ? { configured: true } : {};
  const env = await getExpandedEnv(config.env);
  const binaryPath = await findOnPath(configuredPath || binary, env);
  if (!binaryPath) {
    return { type, name, binary, path: "", version: "", available: false, ...configured };
  }

  const version = await getVersion(binaryPath, versionFlag, env);
  return { type, name, binary, path: binaryPath, version, available: true, ...configured };
}

// Detection and process launches share the resolved login environment, so GUI
// startup never needs a second synchronous shell probe.
async function findOnPath(binary: string, env: Record<string, string>): Promise<string | null> {
  if (path.isAbsolute(binary)) {
    try {
      await fs.access(binary, constants.X_OK);
      return (await fs.stat(binary)).isFile() ? binary : null;
    } catch { return null; }
  }
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
function getVersion(binaryPath: string, versionFlag: string, env: Record<string, string>): Promise<string> {
  return new Promise((resolve) => {
    execFile(binaryPath, [versionFlag], { timeout: 5000, env }, (err, stdout, stderr) => {
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
    processFactory?: AgentProcessFactory;
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
  config?: { env?: Record<string, string>; optOutNotifications?: string[]; processFactory?: AgentProcessFactory },
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
      processFactory: config?.processFactory,
    });
  }
  return _registry.createAdapter(agentType, {
    binaryPath: customBinary,
    extraArgs: customArgs,
    env: config?.env,
    optOutNotifications: config?.optOutNotifications,
    processFactory: config?.processFactory,
  });
}
