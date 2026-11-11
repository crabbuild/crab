import { NativeSelect } from "@/components/ui/native-select";
import { useState } from "react";
import { ChevronDown, ChevronRight, Shield, Zap } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

interface ToolPermissions {
  fileRead: boolean;
  fileWrite: boolean;
  shellCommands: boolean;
  webFetch: boolean;
  mcpTools: boolean;
}

type ReasoningLevel = "low" | "medium" | "high";

interface AgentSessionControls {
  model?: string;
  reasoningLevel?: ReasoningLevel;
  fastMode?: boolean;
  toolPermissions?: ToolPermissions;
}

interface Props {
  controls: AgentSessionControls;
  agentType: string;
  onChange: (controls: AgentSessionControls) => void;
}

// ── Constants ───────────────────────────────────────────────────────

// Keep these catalogs ordered newest → oldest so the first non-default
// option in the picker is always the current flagship. The legacy
// `claude-*-20250514` IDs were deprecated on the Claude API with
// retirement on 2026-06-15 — don't re-add them.
const MODELS_BY_AGENT: Record<string, { value: string; label: string }[]> = {
  "claude-code": [
    { value: "claude-opus-4-7", label: "Claude Opus 4.7" },
    { value: "claude-sonnet-4-6", label: "Claude Sonnet 4.6" },
    { value: "claude-haiku-4-5", label: "Claude Haiku 4.5" },
  ],
  codex: [
    { value: "gpt-5.3-codex", label: "GPT-5.3 Codex" },
    { value: "gpt-5-codex", label: "GPT-5 Codex" },
    { value: "gpt-5.5", label: "GPT-5.5" },
  ],
  custom: [],
};

const REASONING_LEVELS: { value: ReasoningLevel; label: string }[] = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
];

const PERMISSION_LABELS: { key: keyof ToolPermissions; label: string; hint: string }[] = [
  { key: "fileRead", label: "File read", hint: "Read files in the workspace" },
  { key: "fileWrite", label: "File write", hint: "Create and modify files" },
  { key: "shellCommands", label: "Shell commands", hint: "Execute terminal commands" },
  { key: "webFetch", label: "Web fetch", hint: "Make HTTP requests" },
  { key: "mcpTools", label: "MCP tools", hint: "Use MCP server tools" },
];

// ── Component ───────────────────────────────────────────────────────

export function SteeringControls({ controls, agentType, onChange }: Props) {
  const [expanded, setExpanded] = useState(false);
  const [permissionsOpen, setPermissionsOpen] = useState(false);

  const models = MODELS_BY_AGENT[agentType] ?? [];

  const handleModelChange = (model: string) => {
    onChange({ ...controls, model: model || undefined });
  };

  const handleReasoningChange = (level: ReasoningLevel) => {
    onChange({ ...controls, reasoningLevel: level });
  };

  const handleFastModeToggle = () => {
    onChange({ ...controls, fastMode: !controls.fastMode });
  };

  const handlePermissionChange = (key: keyof ToolPermissions, value: boolean) => {
    const current = controls.toolPermissions ?? {
      fileRead: true,
      fileWrite: true,
      shellCommands: true,
      webFetch: true,
      mcpTools: true,
    };
    onChange({
      ...controls,
      toolPermissions: { ...current, [key]: value },
    });
  };

  return (
    <div className="border-b border-border/60">
      {/* Collapse toggle */}
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-1.5 px-3 py-1.5 text-[11px] text-muted-foreground transition hover:text-foreground"
        aria-expanded={expanded}
        aria-controls="steering-controls-panel"
      >
        {expanded ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
        <span>Steering</span>
        {controls.fastMode && (
          <span className="ml-1 rounded-full bg-primary/15 px-1.5 py-0.5 text-[9px] text-primary">
            Fast
          </span>
        )}
        {controls.model && (
          <span className="ml-1 text-[9px] text-muted-foreground/70">
            {models.find((m) => m.value === controls.model)?.label ?? controls.model}
          </span>
        )}
      </button>

      {/* Expanded panel */}
      {expanded && (
        <div
          id="steering-controls-panel"
          className="flex flex-wrap items-center gap-3 px-3 pb-2"
          role="group"
          aria-label="Agent steering controls"
        >
          {/* Model picker */}
          {models.length > 0 && (
            <div className="flex items-center gap-1.5">
              <label
                htmlFor="steering-model"
                className="text-[10px] text-muted-foreground"
              >
                Model
              </label>
              <NativeSelect
                id="steering-model"
                className="rounded border border-border bg-background px-1.5 py-0.5 text-[10px] text-foreground focus-visible:outline-none focus:border-primary"
                value={controls.model ?? ""}
                onChange={(e) => handleModelChange(e.target.value)}
                aria-label="Select model"
              >
                <option value="">Default</option>
                {models.map((m) => (
                  <option key={m.value} value={m.value}>
                    {m.label}
                  </option>
                ))}
              </NativeSelect>
            </div>
          )}

          {/* Reasoning level */}
          <div className="flex items-center gap-1.5">
            <label
              htmlFor="steering-reasoning"
              className="text-[10px] text-muted-foreground"
            >
              Reasoning
            </label>
            <NativeSelect
              id="steering-reasoning"
              className="rounded border border-border bg-background px-1.5 py-0.5 text-[10px] text-foreground focus-visible:outline-none focus:border-primary"
              value={controls.reasoningLevel ?? "medium"}
              onChange={(e) => handleReasoningChange(e.target.value as ReasoningLevel)}
              aria-label="Reasoning level"
            >
              {REASONING_LEVELS.map((l) => (
                <option key={l.value} value={l.value}>
                  {l.label}
                </option>
              ))}
            </NativeSelect>
          </div>

          {/* Fast mode toggle */}
          <button
            type="button"
            onClick={handleFastModeToggle}
            className={`flex items-center gap-1 rounded border px-2 py-0.5 text-[10px] transition ${
              controls.fastMode
                ? "border-primary/60 bg-primary/10 text-primary"
                : "border-border bg-background text-muted-foreground hover:text-foreground"
            }`}
            role="switch"
            aria-checked={controls.fastMode ?? false}
            aria-label="Fast mode"
          >
            <Zap size={10} />
            Fast
          </button>

          {/* Permissions button */}
          <div className="relative">
            <button
              type="button"
              onClick={() => setPermissionsOpen(!permissionsOpen)}
              className="flex items-center gap-1 rounded border border-border bg-background px-2 py-0.5 text-[10px] text-muted-foreground transition hover:text-foreground"
              aria-expanded={permissionsOpen}
              aria-haspopup="dialog"
              aria-label="Tool permissions"
            >
              <Shield size={10} />
              Permissions
            </button>

            {/* Permissions dropdown */}
            {permissionsOpen && (
              <div
                className="absolute left-0 top-full z-10 mt-1 w-56 rounded border border-border bg-background p-3 shadow-lg"
                role="dialog"
                aria-label="Tool permissions"
              >
                <div className="mb-2 text-[10px] font-medium text-foreground">
                  Tool Permissions
                </div>
                <ul className="space-y-1.5" role="list">
                  {PERMISSION_LABELS.map(({ key, label, hint }) => {
                    const checked = controls.toolPermissions?.[key] ?? true;
                    return (
                      <li key={key} className="flex items-center gap-2">
                        <input
                          type="checkbox"
                          id={`perm-${key}`}
                          checked={checked}
                          onChange={(e) => handlePermissionChange(key, e.target.checked)}
                          className="h-3 w-3 rounded border-border accent-primary"
                        />
                        <label
                          htmlFor={`perm-${key}`}
                          className="flex-1 text-[10px] text-foreground"
                          title={hint}
                        >
                          {label}
                        </label>
                      </li>
                    );
                  })}
                </ul>
                <button
                  type="button"
                  onClick={() => setPermissionsOpen(false)}
                  className="mt-2 w-full rounded border border-border px-2 py-1 text-[10px] text-muted-foreground transition hover:bg-accent hover:text-foreground"
                >
                  Done
                </button>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
