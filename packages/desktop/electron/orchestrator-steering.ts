/**
 * Agent Steering & Session Controls for the Crab Desktop Orchestrator.
 *
 * Manages per-workspace session controls that influence how the agent
 * operates: model selection, reasoning level, fast mode, and tool
 * permissions. Controls are persisted in workspace metadata and take
 * effect on the next agent turn.
 */

import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ToolPermissions {
  fileRead: boolean;
  fileWrite: boolean;
  shellCommands: boolean;
  webFetch: boolean;
  mcpTools: boolean;
}

export type ReasoningLevel = "low" | "medium" | "high" | "extra-high";

export interface AgentSessionControls {
  model?: string;
  reasoningLevel?: ReasoningLevel;
  fastMode?: boolean;
  toolPermissions?: ToolPermissions;
}

interface PersistedSteeringData {
  workspaces: Record<string, AgentSessionControls>;
}

/**
 * Maps agent type to supported models for validation. Keep in sync with the
 * frontend catalogs in `SteeringControls.tsx` and `AgentTasksView.tsx`.
 *
 * The legacy Claude IDs `claude-sonnet-4-20250514` / `claude-opus-4-20250514`
 * were deprecated by Anthropic with retirement scheduled for 2026-06-15 —
 * reject them at the steering layer so users can't pin workspaces to a model
 * the upstream API will start rejecting.
 */
const SUPPORTED_MODELS: Record<string, string[]> = {
  "claude-code": [
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
  ],
  codex: [
    "gpt-5.3-codex",
    "gpt-5-codex",
    "gpt-5.5",
  ],
  custom: [],
};

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_PERSIST_PATH = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "agent-steering.json",
);

const DEFAULT_CONTROLS: AgentSessionControls = {
  model: undefined,
  reasoningLevel: "medium",
  fastMode: false,
  toolPermissions: {
    fileRead: true,
    fileWrite: true,
    shellCommands: true,
    webFetch: true,
    mcpTools: true,
  },
};

// ─── AgentSteering ───────────────────────────────────────────────────────────

export class AgentSteering extends EventEmitter {
  private controls = new Map<string, AgentSessionControls>();
  private readonly persistPath: string;

  constructor(persistPath?: string) {
    super();
    this.persistPath = persistPath ?? DEFAULT_PERSIST_PATH;
    this.loadControls();
  }

  // ─── Public API ──────────────────────────────────────────────────────────

  /**
   * Sets session controls for a workspace. Merges with existing controls
   * (partial updates are supported). Changes take effect on the next
   * agent turn.
   */
  async setControls(
    workspaceId: string,
    controls: Partial<AgentSessionControls>,
  ): Promise<AgentSessionControls> {
    const existing = this.controls.get(workspaceId) ?? { ...DEFAULT_CONTROLS };

    const merged: AgentSessionControls = {
      ...existing,
      ...controls,
      toolPermissions: controls.toolPermissions
        ? { ...existing.toolPermissions, ...controls.toolPermissions }
        : existing.toolPermissions,
    };

    this.controls.set(workspaceId, merged);
    await this.persistControls();

    this.emit("orchestrator:steering-updated", { workspaceId, controls: merged });
    return merged;
  }

  /**
   * Gets the session controls for a workspace. Returns defaults if
   * no controls have been explicitly set.
   */
  getControls(workspaceId: string): AgentSessionControls {
    return this.controls.get(workspaceId) ?? { ...DEFAULT_CONTROLS };
  }

  /**
   * Removes session controls for a workspace (resets to defaults).
   */
  async resetControls(workspaceId: string): Promise<void> {
    this.controls.delete(workspaceId);
    await this.persistControls();
  }

  /**
   * Validates that a model is compatible with the given agent type.
   * Returns a warning message if the model is not in the supported list,
   * or null if valid.
   */
  validateModel(model: string, agentType: string): string | null {
    const supported = SUPPORTED_MODELS[agentType];
    if (!supported || supported.length === 0) {
      // Custom agents accept any model.
      return null;
    }

    if (!supported.includes(model)) {
      return `Model "${model}" may not be compatible with ${agentType}. Supported: ${supported.join(", ")}`;
    }

    return null;
  }

  /**
   * Builds CLI flags for the agent based on the current session controls.
   * Used when spawning or re-spawning the agent process.
   */
  buildAgentFlags(workspaceId: string, agentType: string): string[] {
    const controls = this.getControls(workspaceId);
    const flags: string[] = [];

    if (agentType === "claude-code") {
      // Model override.
      if (controls.model) {
        flags.push("--model", controls.model);
      }

      // Reasoning effort → Claude's thinking effort.
      if (controls.reasoningLevel) {
        // Claude Code CLI accepts --thinking-effort with values: low, medium, high.
        flags.push("-c", `thinking_effort="${controls.reasoningLevel}"`);
      }

      // Fast mode.
      if (controls.fastMode) {
        flags.push("--fast");
      }

      // Tool permissions → --allowedTools.
      if (controls.toolPermissions) {
        const allowed = this.buildClaudeAllowedTools(controls.toolPermissions);
        if (allowed.length > 0) {
          flags.push("--allowedTools", allowed.join(","));
        }
      }
    } else if (agentType === "codex") {
      // Model override.
      if (controls.model) {
        flags.push("--model", controls.model);
      }

      // Reasoning effort → Codex's model_reasoning_effort config.
      if (controls.reasoningLevel) {
        flags.push("-c", `model_reasoning_effort="${controls.reasoningLevel}"`);
      }

      // Approval mode based on tool permissions.
      if (controls.toolPermissions) {
        const mode = this.buildCodexApprovalMode(controls.toolPermissions);
        flags.push("--approval-mode", mode);
      }
    }

    return flags;
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Maps tool permissions to Claude Code's --allowedTools flag values.
   */
  private buildClaudeAllowedTools(permissions: ToolPermissions): string[] {
    const tools: string[] = [];

    if (permissions.fileRead) tools.push("Read");
    if (permissions.fileWrite) tools.push("Write", "Edit");
    if (permissions.shellCommands) tools.push("Bash");
    if (permissions.webFetch) tools.push("WebFetch");
    if (permissions.mcpTools) tools.push("MCP");

    return tools;
  }

  /**
   * Maps tool permissions to Codex's --approval-mode flag.
   */
  private buildCodexApprovalMode(permissions: ToolPermissions): string {
    // If all permissions are granted, use full-auto.
    if (
      permissions.fileRead &&
      permissions.fileWrite &&
      permissions.shellCommands
    ) {
      return "full-auto";
    }

    // If only read is allowed, use suggest mode.
    if (permissions.fileRead && !permissions.fileWrite && !permissions.shellCommands) {
      return "suggest";
    }

    // Default: auto-edit (allows file changes but prompts for shell).
    return "auto-edit";
  }

  // ─── Persistence ─────────────────────────────────────────────────────────

  private async persistControls(): Promise<void> {
    const data: PersistedSteeringData = {
      workspaces: Object.fromEntries(this.controls),
    };

    const dir = path.dirname(this.persistPath);
    try {
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(this.persistPath, JSON.stringify(data, null, 2), "utf-8");
    } catch (err) {
      this.emit("orchestrator:error", {
        message: `Failed to persist steering controls: ${err instanceof Error ? err.message : String(err)}`,
      });
    }
  }

  private loadControls(): void {
    try {
      if (!fs.existsSync(this.persistPath)) return;

      const raw = fs.readFileSync(this.persistPath, "utf-8");
      const data: PersistedSteeringData = JSON.parse(raw);

      if (!data.workspaces || typeof data.workspaces !== "object") return;

      for (const [workspaceId, controls] of Object.entries(data.workspaces)) {
        this.controls.set(workspaceId, controls);
      }
    } catch {
      // Corrupted or unreadable — start fresh.
    }
  }
}
