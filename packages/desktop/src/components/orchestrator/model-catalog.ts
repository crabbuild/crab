/**
 * Agent model catalog.
 *
 * This is the single source of truth for available models per agent.
 * Update this file when new models are released.
 *
 * Both the NewChatLanding and AgentTasksView (ChatTab) import from here
 * so the model dropdowns stay in sync.
 *
 * Last updated: 2026-05-17
 */

// ── Types ───────────────────────────────────────────────────────────

export type IntelligenceLevel = "low" | "medium" | "high" | "extra-high";

export interface ModelEntry {
  /** Value passed to the agent CLI (e.g. "gpt-5.5", "claude-sonnet-4-6"). */
  value: string;
  /** Display label in the dropdown. */
  label: string;
  /** Short description shown below the label. */
  description?: string;
  /** Tier for visual grouping and default selection. */
  tier?: "recommended" | "fast" | "powerful" | "other";
  /** Available intelligence levels for this model. When set, the user
   *  can pick how much reasoning effort the model should use. */
  intelligenceLevels?: IntelligenceLevel[];
  /** Default intelligence level (if levels are available). */
  defaultIntelligence?: IntelligenceLevel;
}

// ── Codex (OpenAI) ──────────────────────────────────────────────────
//
// Source: Codex app model picker + openai.com/index (May 2026).
// GPT-5.5 is the latest flagship. GPT-5-Codex is the default for Codex.
// Each model supports intelligence levels (reasoning effort).

const CODEX_MODELS: ModelEntry[] = [
  {
    value: "gpt-5.5",
    label: "GPT-5.5",
    description: "Latest flagship model",
    tier: "powerful",
    intelligenceLevels: ["low", "medium", "high", "extra-high"],
    defaultIntelligence: "high",
  },
  {
    value: "gpt-5.4",
    label: "GPT-5.4",
    description: "Strong general-purpose",
    tier: "other",
    intelligenceLevels: ["low", "medium", "high", "extra-high"],
    defaultIntelligence: "high",
  },
  {
    value: "gpt-5-codex",
    label: "GPT-5 Codex",
    description: "Default for Codex tasks",
    tier: "recommended",
    intelligenceLevels: ["low", "medium", "high", "extra-high"],
    defaultIntelligence: "high",
  },
  {
    value: "gpt-5.4-mini",
    label: "GPT-5.4 Mini",
    description: "Fast and cost-efficient",
    tier: "fast",
    intelligenceLevels: ["low", "medium", "high"],
    defaultIntelligence: "medium",
  },
  {
    value: "gpt-5.3-codex",
    label: "GPT-5.3 Codex",
    description: "Optimized for agentic coding",
    tier: "other",
    intelligenceLevels: ["low", "medium", "high", "extra-high"],
    defaultIntelligence: "high",
  },
];

// ── Claude Code (Anthropic) ─────────────────────────────────────────
//
// Source: Anthropic docs + Claude Code model picker (May 2026).
// Opus 4.7 is the latest flagship. Sonnet 4.6 is the default workhorse.
// Claude models use "thinking" budget rather than discrete levels,
// but we map to the same UI for consistency.

const CLAUDE_CODE_MODELS: ModelEntry[] = [
  {
    value: "claude-sonnet-4-6",
    label: "Sonnet 4.6",
    description: "Responsive everyday work",
    tier: "recommended",
    intelligenceLevels: ["low", "medium", "high"],
    defaultIntelligence: "medium",
  },
  {
    value: "claude-opus-4-7",
    label: "Opus 4.7",
    description: "Most capable for complex tasks",
    tier: "powerful",
    intelligenceLevels: ["medium", "high", "extra-high"],
    defaultIntelligence: "high",
  },
  {
    value: "claude-haiku-4-5",
    label: "Haiku 4.5",
    description: "Fastest, most efficient",
    tier: "fast",
    intelligenceLevels: ["low", "medium"],
    defaultIntelligence: "low",
  },
];

// ── Registry ────────────────────────────────────────────────────────

const MODEL_CATALOG: Record<string, ModelEntry[]> = {
  codex: CODEX_MODELS,
  "claude-code": CLAUDE_CODE_MODELS,
};

/**
 * Returns the available models for a given agent type.
 * Returns an empty array for unknown/custom agents (they use their own default).
 */
export function modelsForAgent(agentType: string): ModelEntry[] {
  return MODEL_CATALOG[agentType] ?? [];
}

/**
 * Returns a flat list of { value, label } for the ChatInput model dropdown.
 * This is the format expected by the ChatInputModel interface.
 */
export function chatInputModelsForAgent(agentType: string): { value: string; label: string }[] {
  return modelsForAgent(agentType).map((m) => ({ value: m.value, label: m.label }));
}

/** Human-readable label for an intelligence level. */
export function intelligenceLabel(level: IntelligenceLevel): string {
  switch (level) {
    case "low": return "Low";
    case "medium": return "Medium";
    case "high": return "High";
    case "extra-high": return "Extra High";
  }
}
