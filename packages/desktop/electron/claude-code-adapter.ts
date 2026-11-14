import { spawnLocalAgent, type AgentProcessFactory } from "./agent-process";
import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import { AgentAdapter, AgentSubmissionUncertainError, type AgentQuestionInput, type AgentInputResponse, type AgentTurnOptions, type AgentModel } from "./orchestrator-adapters";
import { elicitationRequest, type PendingElicitation } from "./agent-elicitation";
import { ClaudeEventParser } from "./claude-code-events";
import { teamMcpResponse } from "./agent-team";

interface ClaudeApproval {
  input: Record<string, unknown>;
  suggestions: Array<Record<string, unknown>>;
  options: Array<{ id: string; label: string }>;
}

/** Drive the bidirectional stream-json protocol used by the official Agent SDK. */
export class ClaudeCodeAdapter extends AgentAdapter {
  readonly name = "Claude Code";
  readonly binary: string;
  resumeSessionId: string | null = null;
  private models: AgentModel[] = [];
  private initialized = false;
  private cancelled = false;
  private active = false;
  private prompts: string[] = [];
  private initId = "";
  private ready: Promise<void> = Promise.resolve();
  private readyResolve: (() => void) | undefined;
  private readyReject: ((error: Error) => void) | undefined;
  private submission: Promise<void> = Promise.resolve();
  private controls = new Map<string, { resolve: () => void; reject: (error: Error) => void }>();
  private terminateTimer: ReturnType<typeof setTimeout> | undefined;
  private killTimer: ReturnType<typeof setTimeout> | undefined;
  private approvals = new Map<string, ClaudeApproval>();
  private inputs = new Map<string, { input: Record<string, unknown>; questions: AgentQuestionInput["questions"] } | PendingElicitation>();
  private seenRequests = new Set<string>();
  private initialOptions?: AgentTurnOptions;
  private mode?: AgentTurnOptions["mode"];
  private processClosed: Promise<void> = Promise.resolve();

  constructor(private readonly extraArgs: string[] = [], binary = "claude", private readonly processFactory: AgentProcessFactory = spawnLocalAgent) { super(); this.binary = binary; }

  spawn(prompt: string, cwd: string, env?: Record<string, string>, options?: AgentTurnOptions): Promise<void> {
    this.initialOptions = options;
    this.connect(prompt, cwd, env);
    return this.ready;
  }

  async listModels(cwd: string, env?: Record<string, string>): Promise<AgentModel[]> {
    if (!this.proc) this.connect(undefined, cwd, env);
    await this.ready;
    return this.models;
  }

  private connect(prompt: string | undefined, cwd: string, env?: Record<string, string>): void {
    if (this.proc || this.cancelled) throw new Error("Claude Code is already connected or closing");
    this.ready = new Promise((resolve, reject) => { this.readyResolve = resolve; this.readyReject = reject; });
    void this.ready.catch(() => {});
    this.exitEmitted = false;
    this.doneEmitted = false;
    this.initialized = false;
    this.prompts = prompt === undefined ? [] : [prompt];
    const parser = new ClaudeEventParser(cwd);
    const resumeArgs = this.resumeSessionId ? ["--resume", this.resumeSessionId] : [];
    const owner = this.processFactory(this.binary, [
      "--print", "--verbose", "--output-format", "stream-json",
      "--input-format", "stream-json", "--include-partial-messages",
      ...(this.teamTools ? ["--mcp-config", JSON.stringify({ mcpServers: { crab_team: { type: "sdk", name: "crab_team" } } })] : []),
      "--permission-prompt-tool", "stdio", ...this.extraArgs, ...resumeArgs,
    ], { cwd, env });
    const child = owner.process;
    this.processClosed = owner.closed;
    this.proc = child;
    let stderr = "";
    child.stderr?.on("data", (chunk: Buffer) => { stderr = (stderr + chunk.toString()).slice(-8192); });
    const reader = createInterface({ input: child.stdout! });
    reader.once("close", () => {
      if (this.proc !== child || this.cancelled) return;
      if (!this.doneEmitted) this.fail("Claude Code connection closed before the turn completed");
      else this.cancel();
    });
    reader.on("line", line => {
      let raw: unknown;
      try { raw = JSON.parse(line); } catch { return; }
      if (!raw || typeof raw !== "object" || Array.isArray(raw)) return;
      const message = raw as Record<string, unknown>;
      if (this.handleControl(message)) return;
      if (this.cancelled) return;
      if (message.type === "system" && typeof message.permissionMode === "string") {
        this.mode = message.permissionMode === "plan" ? "plan" : "instant";
        this.emitEvent({ type: "configuration_changed", timestamp: Date.now(), configuration: { mode: this.mode } });
      }
      const streamEvent = message.event as Record<string, unknown> | undefined;
      if (!this.active && ((message.type === "assistant" && !message.parent_tool_use_id) || streamEvent?.type === "message_start")) {
        this.active = true;
        this.doneEmitted = false;
        this.emitEvent({ type: "turn_started", timestamp: Date.now() });
      }
      for (const event of parser.parse(message)) {
        if (event.type === "session_started" && event.sessionId) this.resumeSessionId = event.sessionId;
        if (event.type === "done") this.active = false;
        this.emitEvent(event);
      }
    });
    child.stdin?.on("error", error => { if (!this.cancelled) this.fail(error); });
    child.once("error", (error: NodeJS.ErrnoException) => {
      const message = error.code === "ENOENT" ? `Agent binary "${this.binary}" not found on PATH. Install it or configure the path in Settings → Agents.` : error.message;
      if (!this.cancelled) this.fail(new Error(message, { cause: error }));
    });
    child.once("close", code => {
      this.clearTimers();
      reader.close();
      this.proc = null;
      this.initialized = false;
      this.clearRequests();
      if (!this.doneEmitted && (this.active || this.prompts.length)) {
        if (!this.cancelled) this.emitEvent({ type: "error", timestamp: Date.now(), error: stderr.trim() || "Claude Code exited before the turn completed" });
        this.emitEvent({ type: "done", timestamp: Date.now(), outcome: this.cancelled ? "interrupted" : "failed", content: "" });
        this.emitExit(code || 1);
      } else this.emitExit(code ?? 1);
      this.active = false;
      this.prompts = [];
    });
    this.initId = randomUUID();
    // Startup hooks and native controls can outlive a local transport deadline.
    // Task timeouts or the discovery owner cancel the connection and its waits.
    this.write({ type: "control_request", request_id: this.initId, request: { subtype: "initialize", supportedDialogKinds: [], forwardSubagentText: true } });
  }

  private sendMessage(text: string): void {
    if (!this.proc?.stdin?.writable || this.cancelled) throw new Error("No active Claude Code connection");
    if (!this.initialized) { this.prompts.push(text); return; }
    this.doneEmitted = false;
    let written: boolean;
    try {
      written = this.write({ type: "user", message: { role: "user", content: text }, parent_tool_use_id: null,
        ...(this.resumeSessionId ? { session_id: this.resumeSessionId } : {}) });
    } catch (error: unknown) {
      throw new AgentSubmissionUncertainError(error);
    }
    if (!written) throw new Error("No active Claude Code connection");
    if (!this.active) this.emitEvent({ type: "turn_started", timestamp: Date.now() });
    this.active = true;
  }

  iterate(prompt: string, options?: AgentTurnOptions): Promise<void> {
    const request = this.submission.then(() => this.submit(prompt, options));
    this.submission = request.catch(() => {});
    return request;
  }

  private async submit(prompt: string, options?: AgentTurnOptions): Promise<void> {
    await this.ready;
    await this.configure(options);
    this.sendMessage(prompt);
  }

  private async configure(options?: AgentTurnOptions): Promise<void> {
    if (!options) return;
    if (this.active && (options.model !== undefined || options.reasoningLevel !== undefined || (options.mode !== undefined && options.mode !== this.mode))) throw new Error("Wait for the current turn to complete before changing agent settings");
    if (options.mode !== undefined && options.mode !== this.mode) {
      await this.requestControl({ subtype: "set_permission_mode", mode: options.mode === "plan" ? "plan" : "default" });
      this.mode = options.mode;
      this.emitEvent({ type: "configuration_changed", timestamp: Date.now(), configuration: { mode: options.mode } });
    }
    if (options.model !== undefined) {
      await this.requestControl({ subtype: "set_model", ...(options.model === null ? {} : { model: options.model }) });
      this.emitEvent({ type: "configuration_changed", timestamp: Date.now(), configuration: { model: options.model } });
    }
    if (options.reasoningLevel !== undefined) {
      await this.requestControl({ subtype: "apply_flag_settings", settings: { effortLevel: options.reasoningLevel } });
      this.emitEvent({ type: "configuration_changed", timestamp: Date.now(), configuration: { reasoningLevel: options.reasoningLevel } });
    }
  }

  private async finishInitialization(): Promise<void> {
    if (this.initialOptions) await this.configure(this.initialOptions);
    const prompts = this.prompts;
    this.prompts = [];
    for (const prompt of prompts) this.sendMessage(prompt);
    this.readyResolve?.();
  }

  private requestControl(request: object): Promise<void> {
    const id = randomUUID();
    return new Promise((resolve, reject) => {
      this.controls.set(id, { resolve, reject });
      if (!this.write({ type: "control_request", request_id: id, request })) {
        this.controls.delete(id);
        reject(new Error("No active Claude Code connection"));
      }
    });
  }

  respondApproval(requestId: string, decision: string): boolean {
    const pending = this.approvals.get(requestId);
    if (!pending || this.cancelled || !pending.options.some(option => option.id === decision)) return false;
    const response: Record<string, unknown> = decision === "deny"
      ? { behavior: "deny", message: "User denied the tool call" }
      : { behavior: "allow", updatedInput: pending.input };
    if (decision === "allow" && pending.suggestions.length) {
      // The shared Allow action grants this session, not permission changes
      // in the user's project/settings files.
      response.updatedPermissions = pending.suggestions.map(suggestion => ({ ...suggestion, destination: "session" }));
    }
    if (!this.respond(requestId, response)) return false;
    this.approvals.delete(requestId);
    this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
    return true;
  }

  respondInput(requestId: string, response: AgentInputResponse): boolean {
    const pending = this.inputs.get(requestId);
    if (!pending || this.cancelled) return false;
    let result: object;
    if ("request" in pending) {
      if (response.kind !== "elicitation") return false;
      result = pending.respond(response);
    } else {
      if (response.kind !== "questions") return false;
      const answers = response.answers;
      result = Object.keys(answers).length
        ? { behavior: "allow", updatedInput: { ...pending.input, answers: Object.fromEntries(pending.questions.map(question => [question.question, (answers[question.id] ?? []).join(", ")])) } }
        : { behavior: "deny", message: "User cancelled the question" };
    }
    if (!this.respond(requestId, result)) return false;
    this.inputs.delete(requestId);
    this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
    return true;
  }

  private cancellation?: Promise<void>;

  cancel(): Promise<void> {
    if (this.cancelled) return this.cancellation ?? Promise.resolve();
    this.cancelled = true;
    this.teamCalls.abort(new Error("Conversation stopped"));
    const child = this.proc;
    if (!child) return Promise.resolve();
    this.cancellation = this.processClosed;
    if (!this.initialized) child.kill("SIGTERM");
    for (const requestId of this.approvals.keys()) this.respond(requestId, { behavior: "deny", message: "Conversation stopped", interrupt: true });
    for (const [requestId, pending] of this.inputs) this.respond(requestId, "request" in pending ? { action: "cancel" } : { behavior: "deny", message: "Conversation stopped", interrupt: true });
    this.clearRequests();
    if (this.active) this.write({ type: "control_request", request_id: randomUUID(), request: { subtype: "interrupt" } });
    child.stdin?.end();
    this.terminateTimer = setTimeout(() => {
      if (this.proc !== child) return;
      child.kill("SIGTERM");
      this.killTimer = setTimeout(() => { if (this.proc === child) child.kill("SIGKILL"); }, 5000);
    }, 2000);
    return this.cancellation;
  }

  private handleControl(raw: Record<string, unknown>): boolean {
    if (raw.type === "control_response") {
      const response = raw.response as Record<string, unknown> | undefined;
      const controlId = String(response?.request_id);
      const pending = this.controls.get(controlId);
      if (pending) {
        this.controls.delete(controlId);
        if (response?.subtype === "success") pending.resolve();
        else pending.reject(new Error(String(response?.error ?? "Claude Code control request failed")));
        return true;
      }
      if (response?.request_id !== this.initId || this.initialized || this.cancelled) return true;
      if (response.subtype !== "success") { this.fail(String(response.error ?? "Claude Code initialization failed")); return true; }
      const initialization = response.response as { models?: Array<{ value: string; displayName: string; description: string; supportsEffort?: boolean; supportedEffortLevels?: string[] }> } | undefined;
      this.models = (initialization?.models ?? []).map(model => ({
        value: model.value, label: model.displayName, description: model.description,
        intelligenceLevels: model.supportsEffort ? model.supportedEffortLevels ?? [] : [],
      }));
      this.initialized = true;
      // Newer CLIs redeliver pending prompts on initialize. Route those through
      // the live handler so replay races never create a second decision card.
      for (const key of ["pending_permission_requests", "pending_elicitation_requests"] as const) {
        if (Array.isArray(response[key])) {
          for (const frame of response[key]) {
            if (frame && typeof frame === "object" && !Array.isArray(frame)) this.handleControl(frame as Record<string, unknown>);
          }
        }
      }
      void this.finishInitialization().catch((error: unknown) => this.fail(error instanceof Error ? error : new Error("Claude Code configuration failed", { cause: error })));
      return true;
    }
    if (raw.type === "control_cancel_request") {
      const requestId = String(raw.request_id);
      this.seenRequests.add(requestId);
      this.approvals.delete(requestId);
      this.inputs.delete(requestId);
      this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
      return true;
    }
    if (raw.type !== "control_request") return false;
    const requestId = String(raw.request_id);
    const request = raw.request as Record<string, unknown> | undefined;
    if (this.seenRequests.has(requestId)) return true;
    if (!this.cancelled && request?.subtype === "mcp_message" && request.server_name === "crab_team" && this.teamTools) {
      this.seenRequests.add(requestId);
      void teamMcpResponse(this.teamTools, request.message as Record<string, unknown>, this.teamCalls.signal).then(response => {
        if (!this.cancelled) this.write({ type: "control_response", response: { subtype: "success", request_id: requestId, response: { mcp_response: response } } });
      }).catch((error: unknown) => {
        if (!this.cancelled) this.write({ type: "control_response", response: { subtype: "error", request_id: requestId, error: error instanceof Error ? error.message : String(error) } });
      });
      return true;
    }
    if (request?.subtype === "request_user_dialog") {
      // No opaque dialog kinds are advertised. On shared transports an
      // undeclared dialog belongs to another client and must remain unanswered.
      return true;
    }
    if (!this.cancelled && request?.subtype === "elicitation") {
      this.seenRequests.add(requestId);
      try {
        const elicitation = elicitationRequest({ requestId, serverName: String(request.mcp_server_name), message: String(request.message), mode: request.mode, schema: request.requested_schema, url: request.url, propertyOrder: request.property_order });
        this.inputs.set(requestId, elicitation);
        this.emitEvent({ type: "input_request", timestamp: Date.now(), inputRequest: elicitation.request });
      } catch (error: unknown) {
        const message = error instanceof Error ? error.message : "Invalid MCP request";
        this.write({ type: "control_response", response: { subtype: "error", request_id: requestId, error: message } });
        this.emitEvent({ type: "generic", timestamp: Date.now(), tool: "ServerWarning", input: { serverName: request.mcp_server_name, error: message } });
      }
      return true;
    }
    if (this.cancelled || request?.subtype !== "can_use_tool") {
      this.write({ type: "control_response", response: { subtype: "error", request_id: requestId, error: "Unsupported control request" } });
      return true;
    }
    this.seenRequests.add(requestId);
    const tool = String(request.tool_name);
    const input = (request.input ?? {}) as Record<string, unknown>;
    if (tool === "AskUserQuestion" && Array.isArray(input.questions)) {
      const questions = (input.questions as Array<Record<string, unknown>>).map((question, index) => ({
        id: String(index), question: String(question.question), header: String(question.header ?? ""),
        options: question.options as AgentQuestionInput["questions"][number]["options"],
        multiSelect: question.multiSelect === true, allowFreeform: true,
      }));
      this.inputs.set(requestId, { input, questions });
      this.emitEvent({ type: "input_request", timestamp: Date.now(), itemId: String(request.tool_use_id ?? requestId), inputRequest: { requestId, questions } });
      return true;
    }
    const suggestions = Array.isArray(request.permission_suggestions) ? request.permission_suggestions as Array<Record<string, unknown>> : [];
    const options = [
      { id: "allow_once", label: "Allow once" },
      ...(suggestions.length && request.suppress_always_allow_rule !== true ? [{ id: "allow", label: "Allow for session" }] : []),
      { id: "deny", label: "Deny" },
    ];
    this.approvals.set(requestId, { input, suggestions, options });
    const kind = tool === "Bash" ? "command" : ["Write", "Edit", "NotebookEdit"].includes(tool) ? "file" : "other";
    this.emitEvent({ type: "approval_request", timestamp: Date.now(), approval: {
      requestId, kind, summary: `Allow ${tool}`, detail: JSON.stringify(input, null, 2), blocking: true, options,
    } });
    return true;
  }

  private write(frame: object): boolean {
    if (!this.proc?.stdin?.writable) return false;
    this.proc.stdin.write(JSON.stringify(frame) + "\n");
    return true;
  }

  private respond(requestId: string, response: object): boolean {
    return this.write({ type: "control_response", response: { subtype: "success", request_id: requestId, response } });
  }

  private fail(error: Error | string): void {
    if (this.exitEmitted) return;
    const failure = typeof error === "string" ? new Error(error) : error;
    this.readyReject?.(failure);
    this.emitEvent({ type: "error", timestamp: Date.now(), error: failure.message });
    this.emitEvent({ type: "done", timestamp: Date.now(), outcome: "failed", content: "" });
    this.cancel();
  }

  private clearTimers(): void {
    clearTimeout(this.terminateTimer);
    clearTimeout(this.killTimer);
  }

  private clearRequests(): void {
    const error = new Error("Claude Code connection closed");
    this.readyReject?.(error);
    for (const pending of this.controls.values()) pending.reject(error);
    this.controls.clear();
    // Resolve the rendered cards when the connection ends; stale controls
    // must not offer to answer requests that can no longer reach Claude.
    for (const requestId of [...this.approvals.keys(), ...this.inputs.keys()]) {
      this.emitEvent({ type: "request_resolved", timestamp: Date.now(), requestId });
    }
    this.approvals.clear();
    this.inputs.clear();
  }
}
