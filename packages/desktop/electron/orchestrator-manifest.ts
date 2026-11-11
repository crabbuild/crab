/**
 * Agent Manifest schema, validation, and manifest-driven adapter for the
 * Crab Desktop Orchestrator.
 *
 * A manifest is a declarative JSON description of how to spawn, communicate
 * with, and normalize events from any agent CLI. This eliminates the need
 * to write TypeScript adapter code for each new agent.
 */

import { spawn } from "node:child_process";
import * as readline from "node:readline";
import * as os from "node:os";
import * as path from "node:path";
import * as fs from "node:fs";

import type { AgentCapabilities,AgentConfig } from "./orchestrator-registry";
import { AgentAdapter } from "./orchestrator-adapters";
import type { AgentEvent } from "./orchestrator-adapters";

// ─── AgentManifest Interfaces ────────────────────────────────────────────────

/** Declarative JSON schema describing an agent CLI. */
export interface AgentManifest {
  /** Unique type identifier, e.g. "opencode" */
  type: string;

  /** Human-readable display name */
  name: string;

  /** Binary name or absolute path */
  binary: string;

  /** CLI arguments template. Supports {prompt}, {cwd} placeholders. */
  args: string[];

  /** Environment variables to set when spawning */
  env?: Record<string, string>;

  /** How the agent communicates */
  interactionModel: "stdio" | "http" | "file" | "text";

  /** For http model: port or URL pattern */
  httpEndpoint?: string;

  /** Flag to pass for version detection, e.g. "--version" */
  versionFlag?: string;

  /** How to map agent events to AgentEvent */
  eventMapping: EventMapping;

  /** Agent capabilities */
  capabilities: AgentCapabilities;

  /** Required environment variables (shown in settings UI) */
  requiredEnv?: Array<{
    name: string;
    label: string;
    secret?: boolean;
  }>;

  /** Optional: how to send messages/feedback to the agent */
  inputFormat?: {
    /** JSON template for stdin messages. Supports {message} placeholder. */
    template: string;
  };
}

/** Configures how raw agent events are mapped to the unified AgentEvent schema. */
export interface EventMapping {
  /** Field name in agent's JSON that indicates event type */
  typeField: string;

  /** Map from agent's type values to AgentEvent types */
  typeMap: Record<string, AgentEvent["type"]>;

  /** Map from agent's field names to AgentEvent field names */
  fieldMap: Record<string, string>;

  /** Field path for content/text extraction */
  contentField?: string;

  /** Field path for error messages */
  errorField?: string;

  /** Field path for tool name */
  toolField?: string;

  /** Field path for token usage object */
  usageField?: string;

  /** Nested field mappings for usage data */
  usageMapping?: {
    inputTokens: string;
    outputTokens: string;
    cacheCreationInputTokens?: string;
    cacheReadInputTokens?: string;
  };
}

// ─── Manifest Validation ─────────────────────────────────────────────────────

export type ManifestValidationResult =
  | { valid: true; manifest: AgentManifest }
  | { valid: false; error: { field: string; reason: string } };

const VALID_INTERACTION_MODELS = ["stdio", "http", "file", "text"] as const;

/**
 * Validates an unknown input against the AgentManifest schema.
 * Returns a structured error with the failing field name and reason,
 * or the validated manifest on success.
 */
export function validateManifest(input: unknown): ManifestValidationResult {
  if (input == null || typeof input !== "object") {
    return { valid: false, error: { field: "(root)", reason: "manifest must be a non-null object" } };
  }

  const obj = input as Record<string, unknown>;

  // type: required string
  if (typeof obj.type !== "string" || obj.type.length === 0) {
    return { valid: false, error: { field: "type", reason: "must be a non-empty string" } };
  }

  // name: required string
  if (typeof obj.name !== "string" || obj.name.length === 0) {
    return { valid: false, error: { field: "name", reason: "must be a non-empty string" } };
  }

  // binary: required string
  if (typeof obj.binary !== "string" || obj.binary.length === 0) {
    return { valid: false, error: { field: "binary", reason: "must be a non-empty string" } };
  }

  // args: required string array
  if (!Array.isArray(obj.args)) {
    return { valid: false, error: { field: "args", reason: "must be an array" } };
  }
  for (let i = 0; i < obj.args.length; i++) {
    if (typeof obj.args[i] !== "string") {
      return { valid: false, error: { field: "args", reason: `element at index ${i} must be a string` } };
    }
  }

  // interactionModel: required, one of "stdio" | "http" | "file"
  if (typeof obj.interactionModel !== "string") {
    return { valid: false, error: { field: "interactionModel", reason: "must be a string" } };
  }
  if (!(VALID_INTERACTION_MODELS as readonly string[]).includes(obj.interactionModel)) {
    return {
      valid: false,
      error: {
        field: "interactionModel",
        reason: `must be one of: ${VALID_INTERACTION_MODELS.join(", ")}`,
      },
    };
  }

  // eventMapping: required object with typeField, typeMap, fieldMap
  if (obj.eventMapping == null || typeof obj.eventMapping !== "object") {
    return { valid: false, error: { field: "eventMapping", reason: "must be a non-null object" } };
  }
  const em = obj.eventMapping as Record<string, unknown>;
  if (typeof em.typeField !== "string" || em.typeField.length === 0) {
    return { valid: false, error: { field: "eventMapping.typeField", reason: "must be a non-empty string" } };
  }
  if (em.typeMap == null || typeof em.typeMap !== "object" || Array.isArray(em.typeMap)) {
    return { valid: false, error: { field: "eventMapping.typeMap", reason: "must be a non-null object" } };
  }
  if (em.fieldMap == null || typeof em.fieldMap !== "object" || Array.isArray(em.fieldMap)) {
    return { valid: false, error: { field: "eventMapping.fieldMap", reason: "must be a non-null object" } };
  }

  // capabilities: required object with boolean fields
  if (obj.capabilities == null || typeof obj.capabilities !== "object") {
    return { valid: false, error: { field: "capabilities", reason: "must be a non-null object" } };
  }
  const caps = obj.capabilities as Record<string, unknown>;
  const requiredCapFields = [
    "supportsPlanMode",
    "supportsStreaming",
    "supportsToolUse",
    "supportsIteration",
    "supportsApprovalModes",
  ] as const;
  for (const capField of requiredCapFields) {
    if (typeof caps[capField] !== "boolean") {
      return {
        valid: false,
        error: { field: `capabilities.${capField}`, reason: "must be a boolean" },
      };
    }
  }

  // Optional boolean fields — validate type if present.
  if (caps.terminalOnly !== undefined && typeof caps.terminalOnly !== "boolean") {
    return {
      valid: false,
      error: { field: "capabilities.terminalOnly", reason: "must be a boolean when present" },
    };
  }

  return {
    valid: true,
    manifest: obj as unknown as AgentManifest,
  };
}

// ─── EventNormalizer (Task 3) ────────────────────────────────────────────────

/**
 * Transforms agent-specific NDJSON into unified AgentEvent objects
 * using the mapping defined in the agent's manifest.
 */
export class EventNormalizer {
  private mapping: EventMapping;

  constructor(mapping: EventMapping) {
    this.mapping = mapping;
  }

  /**
   * Normalize a raw JSON object from an agent into an AgentEvent.
   * Unmapped event types become "generic" with raw data in `input`.
   * Missing/zero timestamps are injected with Date.now().
   *
   * When the raw event carries a usage payload, the token data is
   * attached to this event's `usage` field; callers that need a
   * standalone `type: "usage"` event should use `normalizeAll()`.
   */
  normalize(raw: Record<string, unknown>): AgentEvent {
    const rawType = this.extractField(raw, this.mapping.typeField) as string;
    const mappedType = this.mapping.typeMap[rawType] ?? "generic";

    // `||` not `??` — a literal 0 should be replaced with the current time
    // (Property 6: normalized events always have a non-zero timestamp).
    const sourceTimestamp =
      typeof raw.timestamp === "number" ? raw.timestamp : undefined;
    const event: AgentEvent = {
      type: mappedType,
      timestamp: sourceTimestamp && sourceTimestamp > 0 ? sourceTimestamp : Date.now(),
    };

    // Apply generic field mappings FIRST so the specific field extractions
    // (content/error/tool/usage below) take precedence and cannot be
    // overwritten by a stray fieldMap entry.
    for (const [agentField, eventField] of Object.entries(this.mapping.fieldMap)) {
      const value = this.extractField(raw, agentField);
      if (value != null) {
        (event as unknown as Record<string, unknown>)[eventField] = value;
      }
    }

    // Map content
    if (this.mapping.contentField) {
      const content = this.extractField(raw, this.mapping.contentField);
      if (content != null) event.content = String(content);
    }

    // Map error
    if (this.mapping.errorField) {
      const error = this.extractField(raw, this.mapping.errorField);
      if (error != null) event.error = String(error);
    }

    // Map tool
    if (this.mapping.toolField) {
      const tool = this.extractField(raw, this.mapping.toolField);
      if (tool != null) event.tool = String(tool);
    }

    // Map usage (attached to this event for convenience; a separate
    // usage-typed event is emitted by `normalizeAll`).
    if (this.mapping.usageField && this.mapping.usageMapping) {
      const usageObj = this.extractField(raw, this.mapping.usageField);
      if (usageObj && typeof usageObj === "object") {
        event.usage = this.normalizeUsage(usageObj as Record<string, unknown>);
      }
    }

    // For generic type, preserve raw data
    if (mappedType === "generic") {
      event.input = raw;
    }

    return event;
  }

  /**
   * Normalize a raw event into one or more AgentEvents. Returns the
   * primary normalized event plus a standalone `type: "usage"` event
   * when token usage data is present, matching the behaviour of the
   * built-in Claude Code and Codex adapters so the cost tracker sees
   * every usage payload identically.
   */
  normalizeAll(raw: Record<string, unknown>): AgentEvent[] {
    const primary = this.normalize(raw);
    const events: AgentEvent[] = [primary];

    if (this.mapping.usageField && this.mapping.usageMapping) {
      const usageObj = this.extractField(raw, this.mapping.usageField);
      if (usageObj && typeof usageObj === "object") {
        const usage = this.normalizeUsage(usageObj as Record<string, unknown>);
        const nonZero = usage && (
          usage.inputTokens > 0 ||
          usage.outputTokens > 0 ||
          usage.cacheCreationInputTokens > 0 ||
          usage.cacheReadInputTokens > 0
        );
        if (nonZero) {
          events.push({
            type: "usage",
            timestamp: primary.timestamp,
            usage,
          });
        }
      }
    }

    return events;
  }

  private extractField(obj: Record<string, unknown>, path: string): unknown {
    const parts = path.split(".");
    let current: unknown = obj;
    for (const part of parts) {
      if (current == null || typeof current !== "object") return undefined;
      current = (current as Record<string, unknown>)[part];
    }
    return current;
  }

  private normalizeUsage(raw: Record<string, unknown>): AgentEvent["usage"] {
    const mapping = this.mapping.usageMapping!;
    return {
      inputTokens: typeof raw[mapping.inputTokens] === "number" ? raw[mapping.inputTokens] as number : 0,
      outputTokens: typeof raw[mapping.outputTokens] === "number" ? raw[mapping.outputTokens] as number : 0,
      cacheCreationInputTokens: mapping.cacheCreationInputTokens
        ? (typeof raw[mapping.cacheCreationInputTokens] === "number" ? raw[mapping.cacheCreationInputTokens] as number : 0)
        : 0,
      cacheReadInputTokens: mapping.cacheReadInputTokens
        ? (typeof raw[mapping.cacheReadInputTokens] === "number" ? raw[mapping.cacheReadInputTokens] as number : 0)
        : 0,
    };
  }
}

// ─── ManifestAdapter ─────────────────────────────────────────────────────────

/**
 * A generic adapter that interprets an AgentManifest at runtime,
 * eliminating the need to write TypeScript adapter code for each new agent.
 *
 * Supports three interaction models:
 * - stdio: NDJSON on stdin/stdout
 * - http: local HTTP endpoint polling
 * - file: file-based IPC via a temp directory
 */
export class ManifestAdapter extends AgentAdapter {
  readonly name: string;
  readonly binary: string;
  private manifest: AgentManifest;
  private normalizer: EventNormalizer;

  constructor(manifest: AgentManifest, config?: AgentConfig) {
    super();
    this.manifest = manifest;
    this.name = manifest.name;
    this.binary = config?.binaryPath ?? manifest.binary;
    this.normalizer = new EventNormalizer(manifest.eventMapping);
  }

  async spawn(prompt: string, cwd: string, env?: Record<string, string>): Promise<void> {
    const resolvedArgs = this.manifest.args.map(arg =>
      arg.replace("{prompt}", prompt).replace("{cwd}", cwd)
    );
    const mergedEnv = await this.prepareEnvironment({ ...this.manifest.env, ...env });
    if (!mergedEnv) return;

    switch (this.manifest.interactionModel) {
      case "stdio":
        this.spawnStdio(resolvedArgs, cwd, mergedEnv);
        break;
      case "http":
        this.spawnHttp(resolvedArgs, cwd, mergedEnv, prompt);
        break;
      case "file":
        this.spawnFile(resolvedArgs, cwd, mergedEnv, prompt);
        break;
      case "text":
        this.spawnText(resolvedArgs, cwd, mergedEnv);
        break;
    }
  }

  sendMessage(text: string): void {
    if (this.manifest.inputFormat && this.proc?.stdin?.writable) {
      const msg = this.manifest.inputFormat.template.replace("{message}", text);
      this.proc.stdin.write(msg + "\n");
    }
  }

  /**
   * Stdio interaction model: spawn the binary, parse NDJSON from stdout
   * through the EventNormalizer, and emit unified AgentEvents. When a
   * line carries token usage, a standalone `type: "usage"` event is
   * also emitted so the cost tracker sees it regardless of which
   * agent produced the line.
   */
  private spawnStdio(args: string[], cwd: string, env: Record<string, string>): void {
    this.proc = spawn(this.binary, args, {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
    });

    const rl = readline.createInterface({ input: this.proc.stdout! });
    rl.on("line", (line) => {
      if (!line.trim()) return;
      try {
        const raw = JSON.parse(line) as Record<string, unknown>;
        const events = this.normalizer.normalizeAll(raw);
        for (const event of events) {
          this.emitEvent(event);
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

  /**
   * Text interaction model: spawn the binary and forward every non-empty
   * stdout line as a plain `text` AgentEvent. Used for agents like Aider
   * that don't emit structured JSON — the UI surfaces the raw output
   * and file changes are reconciled via git diff on exit.
   */
  private spawnText(args: string[], cwd: string, env: Record<string, string>): void {
    this.proc = spawn(this.binary, args, {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
    });

    const rl = readline.createInterface({ input: this.proc.stdout! });
    rl.on("line", (line) => {
      const trimmed = line.trim();
      if (!trimmed) return;
      this.emitEvent({ type: "text", timestamp: Date.now(), content: line });
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

  /**
   * HTTP interaction model: spawn the binary (which starts a local HTTP server),
   * then poll the configured endpoint for events.
   */
  private spawnHttp(args: string[], cwd: string, env: Record<string, string>, prompt: string): void {
    this.proc = spawn(this.binary, args, {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
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

    // Resolve the HTTP endpoint URL with placeholders
    const endpoint = (this.manifest.httpEndpoint ?? "http://localhost:4096")
      .replace("{cwd}", cwd);

    // Give the server a moment to start, then begin polling
    const pollDelay = 1000;
    const pollInterval = 500;

    setTimeout(() => {
      this.pollHttpEndpoint(endpoint, prompt, pollInterval);
    }, pollDelay);
  }

  /**
   * Polls the HTTP endpoint for events until the process exits.
   */
  private pollHttpEndpoint(endpoint: string, _prompt: string, intervalMs: number): void {
    if (!this.proc) return;

    const poll = async () => {
      if (!this.proc) return;

      try {
        const response = await fetch(`${endpoint}/events`, {
          method: "GET",
          headers: { "Accept": "application/json" },
        });

        if (response.ok) {
          const text = await response.text();
          const lines = text.split("\n").filter(l => l.trim());
          for (const line of lines) {
            try {
              // SSE lines arrive as "data: {...}"; strip the prefix if present.
              const payload = line.startsWith("data:") ? line.slice(5).trim() : line;
              if (!payload || payload === "[DONE]") continue;
              const raw = JSON.parse(payload) as Record<string, unknown>;
              const events = this.normalizer.normalizeAll(raw);
              for (const event of events) {
                this.emitEvent(event);
                // Stop polling if we get a done event
                if (event.type === "done") return;
              }
            } catch {
              // Skip malformed lines
            }
          }
        }
      } catch {
        // Connection refused or network error — server may not be ready yet
      }

      // Schedule next poll if process is still running
      if (this.proc) {
        setTimeout(poll, intervalMs);
      }
    };

    poll();
  }

  /**
   * File interaction model: spawn the binary, watch a temp directory for
   * output files containing agent events.
   */
  private spawnFile(args: string[], cwd: string, env: Record<string, string>, _prompt: string): void {
    // Create a temp directory for file-based IPC
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-agent-"));

    // Add the temp dir to env so the agent knows where to write
    const fileEnv = { ...env, AGENT_OUTPUT_DIR: tmpDir };

    this.proc = spawn(this.binary, args, {
      cwd,
      env: fileEnv,
      stdio: ["pipe", "pipe", "pipe"],
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

    // Watch the temp directory for output files
    this.watchOutputDir(tmpDir);
  }

  /**
   * Watches a temp directory for new files and processes them as agent events.
   */
  private watchOutputDir(dir: string): void {
    let watcher: fs.FSWatcher | null = null;

    try {
      watcher = fs.watch(dir, (eventType, filename) => {
        if (eventType !== "rename" || !filename) return;

        const filePath = path.join(dir, filename);
        try {
          if (!fs.existsSync(filePath)) return;
          const content = fs.readFileSync(filePath, "utf-8");
          const lines = content.split("\n").filter(l => l.trim());

          for (const line of lines) {
            try {
              const raw = JSON.parse(line) as Record<string, unknown>;
              const events = this.normalizer.normalizeAll(raw);
              for (const event of events) {
                this.emitEvent(event);
              }
            } catch {
              // Skip malformed lines
            }
          }
        } catch {
          // File read error — skip
        }
      });
    } catch {
      // Watch setup failed
      this.emitEvent({
        type: "error",
        timestamp: Date.now(),
        error: `Failed to watch output directory: ${dir}`,
      });
    }

    // Clean up watcher when process exits
    this.onExit(() => {
      if (watcher) {
        watcher.close();
        watcher = null;
      }
      // Clean up temp directory
      try {
        fs.rmSync(dir, { recursive: true, force: true });
      } catch {
        // Best-effort cleanup
      }
    });
  }
}
