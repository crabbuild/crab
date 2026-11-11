/**
 * Agent Registry for the Crab Desktop Orchestrator.
 *
 * Provides a pluggable registry that maps agent type identifiers to
 * adapter factories. Built-in agents (claude-code, codex, custom) are
 * registered at module load time. User manifests can extend or override
 * registrations at runtime.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import { AgentAdapter } from "./orchestrator-adapters";
import { GenericAdapter, _setRegistry } from "./orchestrator-adapters";
import { ClaudeCodeAdapter } from "./claude-code-adapter";
import { CodexAppServerAdapter } from "./codex-app-server-adapter";
import { validateManifest, ManifestAdapter } from "./orchestrator-manifest";
import type { AgentProcessFactory } from "./agent-process";
import type { AgentManifest } from "./orchestrator-manifest";

// ─── Interfaces ──────────────────────────────────────────────────────────────

/** Configuration passed to an adapter factory when creating an instance. */
export interface AgentConfig {
  binaryPath?: string;
  extraArgs?: string[];
  env?: Record<string, string>;
  /** Codex-specific: notification methods to suppress on the connection. */
  optOutNotifications?: string[];
  processFactory?: AgentProcessFactory;
}

/** Declares what features an agent supports. */
export interface AgentCapabilities {
  supportsPlanMode: boolean;
  supportsStreaming: boolean;
  supportsToolUse: boolean;
  supportsIteration: boolean;
  supportsApprovalModes: boolean;
  /**
   * When true, the agent does not support the structured NDJSON protocol
   * used by the orchestrator's headless adapter. Instead, it should be
   * spawned in an interactive terminal so the user can manage execution
   * directly. The orchestrator input box will serialize context (prompt,
   * attachments, worktree) into CLI arguments and open a terminal tab.
   */
  terminalOnly?: boolean;
}

// Re-export AgentManifest from its canonical definition.
export type { AgentManifest } from "./orchestrator-manifest";

/** A registered agent type with its factory and metadata. */
export interface AgentRegistration {
  type: string;
  name: string;
  factory: (config?: AgentConfig) => AgentAdapter;
  capabilities: AgentCapabilities;
  manifest?: AgentManifest;
  builtin: boolean;
}

// ─── AgentRegistry ───────────────────────────────────────────────────────────

export class AgentRegistry {
  private registrations = new Map<string, AgentRegistration>();

  /**
   * Register an agent type. Throws if the type is already registered —
   * use override() to replace an existing registration.
   */
  register(registration: AgentRegistration): void {
    if (this.registrations.has(registration.type)) {
      throw new Error(
        `Agent type "${registration.type}" is already registered. Use override() to replace it.`,
      );
    }
    this.registrations.set(registration.type, registration);
  }

  /**
   * Replace an existing registration (e.g. when a user manifest overrides
   * a built-in agent).
   */
  override(registration: AgentRegistration): void {
    this.registrations.set(registration.type, registration);
  }

  /**
   * Create an adapter instance for the given agent type.
   * Throws a descriptive error listing all registered types if not found.
   */
  createAdapter(type: string, config?: AgentConfig): AgentAdapter {
    const registration = this.registrations.get(type);
    if (!registration) {
      const registered = [...this.registrations.keys()].join(", ");
      throw new Error(
        `Unknown agent type "${type}". Registered types: ${registered}`,
      );
    }
    return registration.factory(config);
  }

  /** Returns capabilities for a registered agent type. */
  getCapabilities(type: string): AgentCapabilities {
    const registration = this.registrations.get(type);
    if (!registration) {
      const registered = [...this.registrations.keys()].join(", ");
      throw new Error(
        `Unknown agent type "${type}". Registered types: ${registered}`,
      );
    }
    return registration.capabilities;
  }

  /** Returns all registrations as an array. */
  listRegistered(): AgentRegistration[] {
    return [...this.registrations.values()];
  }

  /** Checks if a type is registered. */
  has(type: string): boolean {
    return this.registrations.has(type);
  }

  /**
   * Load and validate manifest files from a directory.
   * Reads all *.json files, validates each against the AgentManifest schema,
   * and registers valid manifests (using override if the type matches a built-in).
   * Invalid manifests are logged as warnings and skipped.
   * If the directory doesn't exist, returns silently.
   */
  loadManifests(dir: string): void {
    let entries: string[];
    try {
      entries = fs.readdirSync(dir);
    } catch {
      // Directory doesn't exist or is unreadable — return silently
      return;
    }

    const jsonFiles = entries.filter(f => f.endsWith(".json"));

    for (const file of jsonFiles) {
      const filePath = path.join(dir, file);
      try {
        const content = fs.readFileSync(filePath, "utf-8");
        const parsed = JSON.parse(content);
        const result = validateManifest(parsed);

        if (!result.valid) {
          console.warn(
            `[AgentRegistry] Invalid manifest "${file}": ${result.error.field} — ${result.error.reason}`,
          );
          continue;
        }

        const manifest = result.manifest;
        const registration: AgentRegistration = {
          type: manifest.type,
          name: manifest.name,
          factory: (config?: AgentConfig) => new ManifestAdapter(manifest, config),
          capabilities: manifest.capabilities,
          manifest,
          builtin: false,
        };

        if (this.registrations.has(manifest.type)) {
          this.override(registration);
        } else {
          this.registrations.set(manifest.type, registration);
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        console.warn(`[AgentRegistry] Failed to load manifest "${file}": ${message}`);
      }
    }
  }
}

// ─── Built-in Registrations ──────────────────────────────────────────────────

export const agentRegistry = new AgentRegistry();

agentRegistry.register({
  type: "claude-code",
  name: "Claude Code",
  factory: (config?: AgentConfig) => new ClaudeCodeAdapter(config?.extraArgs, config?.binaryPath, config?.processFactory),
  capabilities: {
    supportsPlanMode: true,
    supportsStreaming: true,
    supportsToolUse: true,
    supportsIteration: true,
    supportsApprovalModes: true,
  },
  builtin: true,
});

agentRegistry.register({
  type: "codex",
  name: "Codex",
  factory: (config?: AgentConfig) =>
    new CodexAppServerAdapter(config?.extraArgs, config?.optOutNotifications, config?.binaryPath, config?.processFactory),
  capabilities: {
    supportsPlanMode: true,
    supportsStreaming: true,
    supportsToolUse: true,
    supportsIteration: true,
    supportsApprovalModes: true,
  },
  builtin: true,
});

agentRegistry.register({
  type: "custom",
  name: "Custom Agent",
  factory: (config?: AgentConfig) => {
    if (!config?.binaryPath) {
      throw new Error(
        'Custom agent requires a binaryPath in config. Set it in Settings → Agents.',
      );
    }
    return new GenericAdapter(config.binaryPath, undefined, config.extraArgs);
  },
  capabilities: {
    supportsPlanMode: false,
    supportsStreaming: true,
    supportsToolUse: false,
    supportsIteration: false,
    supportsApprovalModes: false,
  },
  builtin: true,
});

// ─── Load Manifests ──────────────────────────────────────────────────────────

// Load built-in manifests from the app bundle (before user manifests so users can override)
agentRegistry.loadManifests(path.join(__dirname, "manifests"));

// Load user manifests (overrides built-in manifests with the same type)
agentRegistry.loadManifests(path.join(os.homedir(), ".crab", "agents"));

// ─── Wire Registry into Adapters Module ──────────────────────────────────────

// Inject the registry reference into orchestrator-adapters to break the
// circular dependency. This must happen after all registrations are complete.
_setRegistry(agentRegistry);
